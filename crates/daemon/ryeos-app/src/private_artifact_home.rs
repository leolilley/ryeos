//! Generic node-private artifact homes for session-owned upstream state.
//!
//! The daemon owns directory identity, modes, creation, and explicit removal.
//! It does not interpret file names or bytes. Integration-specific callers
//! must obtain initial files from admitted signed content before calling this
//! boundary.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const HOME_ROOT: &str = "private-artifact-homes";
const MAX_HOME_ID_BYTES: usize = 128;
const MAX_INITIAL_FILES: usize = 16;
const MAX_INITIAL_FILE_BYTES: usize = 256 * 1024;
const MAX_INITIAL_TOTAL_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_HOME_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_HOME_ENTRIES: usize = 100_000;
const MAX_HOME_DEPTH: usize = 64;
const HOME_TRAVERSAL_BUDGET: lillux::DirectoryTraversalBudget =
    lillux::DirectoryTraversalBudget::new(MAX_HOME_ENTRIES, MAX_HOME_DEPTH);

fn validate_component(label: &str, value: &str, maximum: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || value == "."
        || value == ".."
    {
        bail!("{label} is not a bounded portable path component");
    }
    Ok(())
}

pub fn home_path(runtime_state_dir: &Path, home_id: &str) -> Result<PathBuf> {
    validate_component("private artifact home id", home_id, MAX_HOME_ID_BYTES)?;
    Ok(runtime_state_dir.join(HOME_ROOT).join(home_id))
}

pub fn dedicated_session_home_id(session_id: &str) -> Result<String> {
    if session_id.is_empty() || session_id.len() > 256 || session_id.chars().any(char::is_control) {
        bail!("dedicated session id is not canonical and bounded");
    }
    let digest = lillux::cas::sha256_hex(session_id.as_bytes());
    Ok(format!("session-{}", &digest[..32]))
}

pub fn home_id_for_exact_path(runtime_state_dir: &Path, path: &Path) -> Result<String> {
    let root = runtime_state_dir.join(HOME_ROOT);
    if path.parent() != Some(root.as_path()) {
        bail!("private artifact path is outside the owned home root");
    }
    let home_id = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow::anyhow!("private artifact path has no UTF-8 home id"))?;
    validate_component("private artifact home id", home_id, MAX_HOME_ID_BYTES)?;
    if home_path(runtime_state_dir, home_id)? != path {
        bail!("private artifact path is not canonical");
    }
    Ok(home_id.to_owned())
}

pub fn create(
    runtime_state_dir: &Path,
    home_id: &str,
    initial_files: &BTreeMap<String, Vec<u8>>,
) -> Result<PathBuf> {
    validate_component("private artifact home id", home_id, MAX_HOME_ID_BYTES)?;
    if initial_files.len() > MAX_INITIAL_FILES {
        bail!("private artifact home has too many initial files");
    }
    let mut total = 0usize;
    for (name, bytes) in initial_files {
        validate_component("private artifact file name", name, 128)?;
        if bytes.len() > MAX_INITIAL_FILE_BYTES {
            bail!("private artifact initial file exceeds its byte ceiling");
        }
        total = total
            .checked_add(bytes.len())
            .context("private artifact initial-file byte total overflow")?;
    }
    if total > MAX_INITIAL_TOTAL_BYTES {
        bail!("private artifact initial files exceed their aggregate byte ceiling");
    }

    let root = lillux::PinnedDirectory::open_or_create(&runtime_state_dir.join(HOME_ROOT))
        .context("open node-private artifact-home root")?;
    root.set_mode(0o700)?;
    let home_name = OsString::from(home_id);
    let home = root
        .create_child(&home_name, 0o700)
        .context("create node-private artifact home")?;
    let result = (|| -> Result<()> {
        for (name, bytes) in initial_files {
            let mut file = home.open_regular_create(OsStr::new(name), true, true, 0o600)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        home.sync()?;
        root.sync()?;
        Ok(())
    })();
    if let Err(error) = result {
        let cleanup = home
            .remove_contents_recursive_bounded(HOME_TRAVERSAL_BUDGET)
            .and_then(|()| {
                root.remove_empty_child_if_same(&home_name, &home)
                    .and_then(|removed| {
                        if removed {
                            Ok(())
                        } else {
                            bail!("private artifact home identity changed during rollback")
                        }
                    })
            });
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => {
                error.context(format!("private artifact home rollback failed: {cleanup}"))
            }
        });
    }
    Ok(home.path().to_path_buf())
}

pub fn remove(runtime_state_dir: &Path, home_id: &str) -> Result<bool> {
    validate_component("private artifact home id", home_id, MAX_HOME_ID_BYTES)?;
    let Some(root) = lillux::PinnedDirectory::open(&runtime_state_dir.join(HOME_ROOT))? else {
        return Ok(false);
    };
    let name = OsString::from(home_id);
    let Some(home) = root.open_child_directory(&name)? else {
        return Ok(false);
    };
    home.remove_contents_recursive_bounded(HOME_TRAVERSAL_BUDGET)?;
    let removed = root.remove_empty_child_if_same(&name, &home)?;
    if !removed {
        bail!("private artifact home identity changed during removal");
    }
    root.sync()?;
    Ok(true)
}

/// Removes only an empty, exact-identity home left behind before its durable
/// ownership row was committed. Non-empty homes are deliberately preserved:
/// their contents may be the only remaining upstream credential evidence and
/// therefore require explicit operator recovery rather than inference.
pub fn remove_empty_orphan(runtime_state_dir: &Path, home_id: &str) -> Result<bool> {
    validate_component("private artifact home id", home_id, MAX_HOME_ID_BYTES)?;
    let Some(root) = lillux::PinnedDirectory::open(&runtime_state_dir.join(HOME_ROOT))? else {
        return Ok(false);
    };
    let name = OsString::from(home_id);
    let Some(home) = root.open_child_directory(&name)? else {
        return Ok(false);
    };
    if !home.entries_no_follow_bounded(1)?.is_empty() {
        bail!("private artifact orphan is non-empty and requires explicit recovery");
    }
    let removed = root.remove_empty_child_if_same(&name, &home)?;
    if !removed {
        bail!("private artifact home identity changed during orphan recovery");
    }
    root.sync()?;
    Ok(true)
}

pub fn require_within_default_limit(runtime_state_dir: &Path, home_id: &str) -> Result<u64> {
    let path = home_path(runtime_state_dir, home_id)?;
    let home = lillux::PinnedDirectory::open(&path)?
        .ok_or_else(|| anyhow::anyhow!("private artifact home is missing"))?;
    measure_directory_bounded(&home, HOME_TRAVERSAL_BUDGET, DEFAULT_MAX_HOME_BYTES)
}

fn require_same_home_device(
    root_device: u64,
    entry: &lillux::PinnedDirectoryEntryMetadata,
) -> Result<()> {
    if entry.containing_device != root_device {
        bail!("private artifact home crosses a mounted filesystem");
    }
    Ok(())
}

fn measure_directory_bounded(
    root: &lillux::PinnedDirectory,
    budget: lillux::DirectoryTraversalBudget,
    maximum_bytes: u64,
) -> Result<u64> {
    #[cfg(not(unix))]
    {
        let _ = (root, budget, maximum_bytes);
        bail!("private artifact home traversal is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        fn visit(
            directory: &lillux::PinnedDirectory,
            root_device: u64,
            remaining_entries: &mut usize,
            max_depth: usize,
            depth: usize,
            maximum_bytes: u64,
            bytes: &mut u64,
        ) -> Result<()> {
            if depth > max_depth {
                bail!("private artifact home reached its directory-depth ceiling");
            }
            let observed = directory
                .entries_no_follow_bounded(*remaining_entries)
                .context("enumerate private artifact home within its entry ceiling")?;
            *remaining_entries = remaining_entries
                .checked_sub(observed.len())
                .context("private artifact entry budget underflow")?;
            for entry in observed {
                require_same_home_device(root_device, &entry)?;
                if entry.mode & 0o077 != 0 {
                    bail!("private artifact entry grants group or other permissions");
                }
                let opened = directory
                    .open_entry(&entry.name, false)
                    .context("open private artifact entry without following links")?
                    .ok_or_else(|| anyhow::anyhow!("private artifact entry disappeared"))?;
                match opened {
                    lillux::PinnedDirectoryEntry::Directory(child) => {
                        if entry.entry_type != lillux::PinnedEntryType::Directory
                            || entry.mode & 0o700 != 0o700
                        {
                            bail!("private artifact directory identity or mode changed");
                        }
                        let (device, inode) = child.device_inode()?;
                        if device != root_device
                            || device != entry.containing_device
                            || inode != entry.inode
                        {
                            bail!("private artifact directory identity changed");
                        }
                        visit(
                            &child,
                            root_device,
                            remaining_entries,
                            max_depth,
                            depth + 1,
                            maximum_bytes,
                            bytes,
                        )?;
                    }
                    lillux::PinnedDirectoryEntry::Regular(file) => {
                        if entry.entry_type != lillux::PinnedEntryType::Regular {
                            bail!("private artifact file identity changed");
                        }
                        let metadata = file.metadata()?;
                        if !metadata.is_file()
                            || metadata.dev() != root_device
                            || metadata.dev() != entry.containing_device
                            || metadata.ino() != entry.inode
                            || metadata.mode() & 0o077 != 0
                        {
                            bail!("private artifact file identity or mode changed");
                        }
                        *bytes = bytes
                            .checked_add(metadata.len())
                            .context("private artifact byte count overflow")?;
                        if *bytes > maximum_bytes {
                            bail!("private artifact home reached its byte ceiling");
                        }
                    }
                }
                directory.ensure_entry_observation(&entry)?;
            }
            Ok(())
        }

        let metadata = root.try_clone_descriptor()?.metadata()?;
        if metadata.mode() & 0o077 != 0 || metadata.mode() & 0o700 != 0o700 {
            bail!("private artifact home root is not owner-private and accessible");
        }
        let root_device = metadata.dev();
        let mut remaining_entries = budget.max_entries;
        let mut bytes = 0;
        visit(
            root,
            root_device,
            &mut remaining_entries,
            budget.max_depth,
            0,
            maximum_bytes,
            &mut bytes,
        )?;
        root.ensure_path_binding()?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn creates_opaque_private_files_and_removes_exact_home() {
        let tmp = tempfile::tempdir().unwrap();
        let files = BTreeMap::from([("fixture.conf".to_owned(), b"fixture".to_vec())]);
        let path = create(tmp.path(), "home-one", &files).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(path.join("fixture.conf"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read(path.join("fixture.conf")).unwrap(),
            b"fixture"
        );
        assert!(remove(tmp.path(), "home-one").unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn rejects_path_components_and_existing_home() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(create(tmp.path(), "../escape", &BTreeMap::new()).is_err());
        create(tmp.path(), "home-one", &BTreeMap::new()).unwrap();
        assert!(create(tmp.path(), "home-one", &BTreeMap::new()).is_err());
    }

    #[test]
    fn recovers_only_empty_creation_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = create(tmp.path(), "empty-orphan", &BTreeMap::new()).unwrap();
        assert!(remove_empty_orphan(tmp.path(), "empty-orphan").unwrap());
        assert!(!empty.exists());

        let files = BTreeMap::from([("credential.json".to_owned(), b"opaque".to_vec())]);
        let nonempty = create(tmp.path(), "nonempty-orphan", &files).unwrap();
        assert!(remove_empty_orphan(tmp.path(), "nonempty-orphan").is_err());
        assert!(nonempty.exists());
    }

    #[test]
    fn measures_only_regular_no_follow_home_content() {
        let tmp = tempfile::tempdir().unwrap();
        let files = BTreeMap::from([("fixture.conf".to_owned(), b"fixture".to_vec())]);
        let path = create(tmp.path(), "home-one", &files).unwrap();
        let nested = path.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700)).unwrap();
        let state = nested.join("state");
        std::fs::write(&state, b"state").unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            require_within_default_limit(tmp.path(), "home-one").unwrap(),
            12
        );
        std::os::unix::fs::symlink("fixture.conf", path.join("link")).unwrap();
        assert!(require_within_default_limit(tmp.path(), "home-one").is_err());
    }

    #[test]
    fn bounded_home_walk_rejects_namespace_before_overallocation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create(tmp.path(), "home-one", &BTreeMap::new()).unwrap();
        for name in ["one", "two"] {
            let file = path.join(name);
            std::fs::write(&file, b"x").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let home = lillux::PinnedDirectory::open(&path).unwrap().unwrap();
        assert!(
            measure_directory_bounded(
                &home,
                lillux::DirectoryTraversalBudget::new(1, MAX_HOME_DEPTH),
                DEFAULT_MAX_HOME_BYTES,
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_home_walk_and_removal_reject_excessive_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create(tmp.path(), "home-one", &BTreeMap::new()).unwrap();
        let first = path.join("first");
        let second = first.join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        for directory in [&first, &second] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let home = lillux::PinnedDirectory::open(&path).unwrap().unwrap();
        assert!(
            measure_directory_bounded(
                &home,
                lillux::DirectoryTraversalBudget::new(MAX_HOME_ENTRIES, 1),
                DEFAULT_MAX_HOME_BYTES,
            )
            .is_err()
        );

        let mut current = second;
        for index in 0..=MAX_HOME_DEPTH {
            current = current.join(format!("depth-{index}"));
            std::fs::create_dir(&current).unwrap();
            std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(remove(tmp.path(), "home-one").is_err());
        assert!(path.exists());
    }

    #[test]
    fn bounded_home_walk_rejects_a_mount_device_observation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create(tmp.path(), "home-one", &BTreeMap::new()).unwrap();
        let file = path.join("credential");
        std::fs::write(&file, b"opaque").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let home = lillux::PinnedDirectory::open(&path).unwrap().unwrap();
        let (root_device, _) = home.device_inode().unwrap();
        let mut entry = home.entries_no_follow_bounded(1).unwrap().remove(0);
        entry.containing_device = root_device.wrapping_add(1);
        assert!(require_same_home_device(root_device, &entry).is_err());
    }
}
