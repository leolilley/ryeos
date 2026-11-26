//! Observation of one entry of a directory tree without following links.
//!
//! Callers that must hold a tree to an exact shape (kind, owner, permission
//! bits) read it through these two functions, so the OS-specific metadata
//! interpretation stays in Lillux.

use std::fs::File;
use std::path::Path;

use anyhow::Result;

/// What an entry is, from `lstat`. A symlink, device, fifo or socket is
/// `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeEntryKind {
    Directory,
    RegularFile,
    Other,
}

/// The kind, owner and permission bits of one entry, as `lstat` reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeEntryObservation {
    pub kind: TreeEntryKind,
    /// Numeric owner of the entry.
    pub owner: u32,
    /// Permission bits including setuid, setgid and sticky (mask `0o7777`).
    pub mode: u32,
    /// Size in bytes; meaningful for regular files.
    pub size: u64,
}

impl TreeEntryObservation {
    /// Whether group or other may write the entry.
    pub fn group_or_other_writable(&self) -> bool {
        self.mode & 0o022 != 0
    }
}

/// Observe `path` with `lstat`; a final symlink is reported as `Other`, never
/// followed.
pub fn observe_tree_entry_no_follow(path: &Path) -> Result<TreeEntryObservation> {
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!("tree entry observation is unavailable on this platform")
    }
    #[cfg(unix)]
    {
        use anyhow::Context as _;
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("observe tree entry {}", path.display()))?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            TreeEntryKind::Directory
        } else if file_type.is_file() {
            TreeEntryKind::RegularFile
        } else {
            TreeEntryKind::Other
        };
        Ok(TreeEntryObservation {
            kind,
            owner: metadata.uid(),
            mode: metadata.permissions().mode() & 0o7777,
            size: metadata.len(),
        })
    }
}

/// Open a regular file for reading without following a final symlink, and
/// return what was opened. The descriptor is checked to be a regular file, so
/// a path swapped for a link or special file between observation and open is
/// refused.
pub fn open_tree_regular_file_no_follow(path: &Path) -> Result<(File, TreeEntryObservation)> {
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!("tree file open is unavailable on this platform")
    }
    #[cfg(unix)]
    {
        use anyhow::Context as _;
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)
            .with_context(|| format!("open tree file {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("observe open tree file {}", path.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "tree entry {} is not a regular file",
            path.display()
        );
        let observation = TreeEntryObservation {
            kind: TreeEntryKind::RegularFile,
            owner: metadata.uid(),
            mode: metadata.permissions().mode() & 0o7777,
            size: metadata.len(),
        };
        Ok((file, observation))
    }
}

/// Set `root` and every directory below it to mode `0o755`, whatever umask
/// they were created under. Links are never followed and non-directories are
/// left alone.
pub fn set_tree_directory_modes(root: &Path) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = root;
        anyhow::bail!("tree directory modes are unavailable on this platform")
    }
    #[cfg(unix)]
    {
        use anyhow::Context as _;
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = std::fs::symlink_metadata(root)
            .with_context(|| format!("observe {}", root.display()))?;
        if !metadata.file_type().is_dir() {
            return Ok(());
        }
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("set mode of {}", root.display()))?;
        for entry in std::fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
            set_tree_directory_modes(&entry?.path())?;
        }
        Ok(())
    }
}

/// Helpers that build odd tree shapes for tests in other crates.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::path::Path;

    /// Set the permission bits of `path`.
    pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    /// Create a symbolic link at `link` pointing to `target`.
    pub fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    /// Set the process umask and return the previous one. The umask is
    /// process-wide: call it only in a process that owns it, such as a test
    /// re-executed as its own child.
    pub fn set_process_umask(mask: u32) -> u32 {
        // SAFETY: umask takes and returns a plain mode and cannot fail.
        unsafe { libc::umask(mask as libc::mode_t) as u32 }
    }

    /// Create a named pipe at `path`.
    pub fn make_fifo(path: &Path) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // SAFETY: a valid NUL-terminated path and a plain mode.
        if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}
