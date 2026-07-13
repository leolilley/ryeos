use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn next_temp_sequence() -> u64 {
    TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

pub fn atomic_write(target: &Path, data: &[u8]) -> Result<()> {
    atomic_write_portable(target, data, None)
}

/// Atomically replace `target` with private data.
///
/// On Unix the temporary file is created as `0600` before any bytes are
/// written, so secret material is never briefly exposed under a permissive
/// mode. The file and containing directory are synced before success returns.
pub fn atomic_write_private(target: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        return atomic_write_private_unix(target, data);
    }
    #[cfg(not(unix))]
    {
        atomic_write_portable(target, data, None)
    }
}

/// Remove a file and durably record the directory update. Missing files are
/// already in the requested state.
pub fn remove_file_durable(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            sync_parent_dir(path)?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Recursively remove a directory and durably record its disappearance.
pub fn remove_dir_all_durable(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => {
            sync_parent_dir(path)?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Flush every regular file and directory in a materialized tree.
///
/// Call this before a staged directory is renamed or exchanged into a live
/// namespace. Files are synced before their containing directories, so a
/// successful return establishes a durability barrier for both file contents
/// and the directory entries that make the tree reachable. Symlinks are not
/// followed; syncing their parent directory makes the link entry durable.
pub fn sync_tree_durable(root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("durable tree root must not be a symlink: {}", root.display());
    }
    if metadata.is_file() {
        fs::File::open(root)?.sync_all()?;
        return Ok(());
    }
    if !metadata.is_dir() {
        anyhow::bail!(
            "durable tree root must be a file or directory: {}",
            root.display()
        );
    }
    sync_directory_tree(root)
}

fn sync_directory_tree(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            sync_directory_tree(&path)?;
        } else if file_type.is_file() {
            fs::File::open(&path)?.sync_all()?;
        } else if !file_type.is_symlink() {
            anyhow::bail!("unsupported entry in durable tree: {}", path.display());
        }
    }
    sync_directory_entry(directory)?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory_entry(directory: &Path) -> std::io::Result<()> {
    fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory_entry(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Atomically exchange two existing sibling filesystem entries.
///
/// RyeOS uses this for live bundle generations: the installed path always
/// names either the complete old tree or the complete staged tree. Platforms
/// without an atomic exchange primitive are rejected rather than using a
/// remove-then-rename compatibility path.
#[cfg(target_os = "linux")]
pub fn atomic_exchange_paths(left: &Path, right: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    if left.parent() != right.parent() {
        anyhow::bail!("atomic exchange paths must share a parent directory");
    }
    let left_c = CString::new(left.as_os_str().as_bytes())?;
    let right_c = CString::new(right.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            left_c.as_ptr(),
            libc::AT_FDCWD,
            right_c.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    sync_parent_dir(left)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn atomic_exchange_paths(_left: &Path, _right: &Path) -> Result<()> {
    anyhow::bail!("atomic filesystem exchange is unavailable on this platform")
}

/// Rename a staged entry into place and durably flush its parent directory.
pub fn rename_path_durable(source: &Path, target: &Path) -> Result<()> {
    if source.parent() != target.parent() {
        anyhow::bail!("durable rename paths must share a parent directory");
    }
    fs::rename(source, target)?;
    sync_parent_dir(target)?;
    Ok(())
}

fn atomic_write_portable(target: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    #[cfg(not(unix))]
    let _ = mode;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut last_collision = None;
    for _ in 0..128 {
        let sequence = next_temp_sequence();
        let tmp = target.with_extension(format!("tmp.{}.{sequence}", std::process::id()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }

        let mut file = match options.open(&tmp) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                last_collision = Some(err);
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        let write_result = (|| -> std::io::Result<()> {
            file.write_all(data)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, target)?;
            sync_parent_dir(target)?;
            Ok(())
        })();
        if let Err(err) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(err.into());
        }
        return Ok(());
    }

    Err(last_collision
        .unwrap_or_else(|| std::io::Error::new(ErrorKind::AlreadyExists, "temp file collision"))
        .into())
}

/// Private atomic replacement relative to an already-open final parent.
///
/// Ancestor symlinks remain supported for app-root compatibility, but the
/// final parent itself must be a real directory. Holding its descriptor across
/// create, rename, and fsync prevents a concurrent parent swap from redirecting
/// secret material.
#[cfg(unix)]
fn atomic_write_private_unix(target: &Path, data: &[u8]) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let parent_c = CString::new(parent.as_os_str().as_bytes())?;
    let parent_fd = unsafe {
        libc::open(
            parent_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if parent_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let parent_file = unsafe { fs::File::from_raw_fd(parent_fd) };

    let file_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(ErrorKind::InvalidInput, "atomic target has no file name")
    })?;
    let target_name = CString::new(file_name.as_bytes())?;
    let mut last_collision = None;

    for _ in 0..128 {
        let sequence = next_temp_sequence();
        let tmp_name = CString::new(format!(
            ".{}.tmp.{}.{sequence}",
            file_name.to_string_lossy(),
            std::process::id()
        ))?;
        let fd = unsafe {
            libc::openat(
                parent_file.as_raw_fd(),
                tmp_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::AlreadyExists {
                last_collision = Some(err);
                continue;
            }
            return Err(err.into());
        }

        let mut tmp_file = unsafe { fs::File::from_raw_fd(fd) };
        let write_result = (|| -> std::io::Result<()> {
            tmp_file.write_all(data)?;
            tmp_file.sync_all()?;
            drop(tmp_file);
            let renamed = unsafe {
                libc::renameat(
                    parent_file.as_raw_fd(),
                    tmp_name.as_ptr(),
                    parent_file.as_raw_fd(),
                    target_name.as_ptr(),
                )
            };
            if renamed != 0 {
                return Err(std::io::Error::last_os_error());
            }
            parent_file.sync_all()
        })();

        if let Err(err) = write_result {
            unsafe {
                libc::unlinkat(parent_file.as_raw_fd(), tmp_name.as_ptr(), 0);
            }
            return Err(err.into());
        }
        return Ok(());
    }

    Err(last_collision
        .unwrap_or_else(|| std::io::Error::new(ErrorKind::AlreadyExists, "temp file collision"))
        .into())
}

#[cfg(unix)]
fn sync_parent_dir(target: &Path) -> std::io::Result<()> {
    fs::File::open(target.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_target: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_replace_writes_complete_value_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.pem");
        atomic_write_private(&target, b"old").unwrap();
        atomic_write_private(&target, b"new-value").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new-value");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], target.file_name().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn private_replace_creates_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.pem");
        atomic_write_private(&target, b"secret").unwrap();
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_replace_rejects_symlink_as_final_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real");
        fs::create_dir(&real_parent).unwrap();
        let linked_parent = dir.path().join("linked");
        symlink(&real_parent, &linked_parent).unwrap();
        let result = atomic_write_private(&linked_parent.join("secret.pem"), b"secret");
        assert!(result.is_err());
        assert!(!real_parent.join("secret.pem").exists());
    }
}
