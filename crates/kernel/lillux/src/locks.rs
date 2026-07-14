use std::path::Path;

use anyhow::Result;

#[cfg(unix)]
use std::fs;

/// Hold an interprocess lock associated with `target` for the entire operation.
/// Callers performing read-modify-write must place the read inside this scope.
pub fn with_exclusive_file_lock<T>(
    target: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    #[cfg(unix)]
    {
        let lock = ExclusiveFileLock::acquire(target)?;
        let result = operation();
        drop(lock);
        result
    }

    #[cfg(not(unix))]
    {
        let _ = (target, operation);
        anyhow::bail!("interprocess file locking is unavailable on this platform")
    }
}

/// An interprocess lock held until the value is dropped.
///
/// Use this instead of [`with_exclusive_file_lock`] when one logical mutation
/// spans async work or several independently fallible phases.
pub struct ExclusiveFileLock {
    #[cfg(unix)]
    _file: fs::File,
    #[cfg(unix)]
    _parent: fs::File,
}

impl ExclusiveFileLock {
    pub fn acquire(target: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::ffi::OsStrExt;

            let parent_path = target.parent().unwrap_or_else(|| Path::new("."));
            fs::create_dir_all(parent_path)?;
            let parent_path = CString::new(parent_path.as_os_str().as_bytes())?;
            let parent_fd = unsafe {
                libc::open(
                    parent_path.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if parent_fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let parent = unsafe { fs::File::from_raw_fd(parent_fd) };
            let file_name = target
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("lock target has no file name"))?;
            let lock_name = CString::new(format!(".{}.lock", file_name.to_string_lossy()))?;
            let file_fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    lock_name.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            if file_fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { fs::File::from_raw_fd(file_fd) };
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self {
                _file: file,
                _parent: parent,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = target;
            anyhow::bail!("interprocess file locking is unavailable on this platform")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn lock_rejects_symlink_as_final_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real");
        fs::create_dir(&real_parent).unwrap();
        let linked_parent = dir.path().join("linked");
        symlink(&real_parent, &linked_parent).unwrap();

        let result = ExclusiveFileLock::acquire(&linked_parent.join("target"));

        assert!(result.is_err());
        assert!(!real_parent.join(".target.lock").exists());
    }
}
