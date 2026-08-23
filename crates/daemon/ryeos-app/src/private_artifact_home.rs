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
        let cleanup = home.remove_contents_recursive().and_then(|()| {
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
    home.remove_contents_recursive()?;
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
    if !home.entries_no_follow()?.is_empty() {
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
    let mut entries = 0usize;
    let bytes = measure_directory(&home, &mut entries)?;
    if bytes > DEFAULT_MAX_HOME_BYTES {
        bail!("private artifact home reached its byte ceiling");
    }
    Ok(bytes)
}

fn measure_directory(directory: &lillux::PinnedDirectory, entries: &mut usize) -> Result<u64> {
    let mut bytes = 0u64;
    for entry in directory.entries_no_follow()? {
        *entries = entries
            .checked_add(1)
            .context("private artifact entry count overflow")?;
        if *entries > MAX_HOME_ENTRIES {
            bail!("private artifact home reached its entry ceiling");
        }
        if entry.mode & 0o077 != 0 {
            bail!("private artifact entry grants group or other permissions");
        }
        if entry.entry_type == lillux::PinnedEntryType::Directory && entry.mode & 0o700 != 0o700 {
            bail!("private artifact directory is not owner-accessible");
        }
        match directory.open_entry(&entry.name, false)? {
            Some(lillux::PinnedDirectoryEntry::Directory(child)) => {
                bytes = bytes
                    .checked_add(measure_directory(&child, entries)?)
                    .context("private artifact byte count overflow")?;
            }
            Some(lillux::PinnedDirectoryEntry::Regular(file)) => {
                bytes = bytes
                    .checked_add(file.metadata()?.len())
                    .context("private artifact byte count overflow")?;
            }
            None => continue,
        }
        if bytes > DEFAULT_MAX_HOME_BYTES {
            bail!("private artifact home reached its byte ceiling");
        }
    }
    Ok(bytes)
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
        std::fs::create_dir(path.join("nested")).unwrap();
        std::fs::write(path.join("nested/state"), b"state").unwrap();
        assert_eq!(
            require_within_default_limit(tmp.path(), "home-one").unwrap(),
            12
        );
        std::os::unix::fs::symlink("fixture.conf", path.join("link")).unwrap();
        assert!(require_within_default_limit(tmp.path(), "home-one").is_err());
    }
}
