use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::Result;

/// Hold an interprocess lock associated with `target` for the entire operation.
/// Callers performing read-modify-write must place the read inside this scope.
pub fn with_exclusive_file_lock<T>(
    target: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let parent = target.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let file_name = target.file_name().ok_or_else(|| {
            std::io::Error::new(ErrorKind::InvalidInput, "lock target has no file name")
        })?;
        let lock_path = parent.join(format!(".{}.lock", file_name.to_string_lossy()));
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&lock_path)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = operation();
        drop(lock);
        return result;
    }

    #[cfg(not(unix))]
    operation()
}

/// An interprocess lock held until the value is dropped.
///
/// Use this instead of [`with_exclusive_file_lock`] when one logical mutation
/// spans async work or several independently fallible phases.
pub struct ExclusiveFileLock {
    #[cfg(unix)]
    _file: fs::File,
}

impl ExclusiveFileLock {
    pub fn acquire(target: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;

            let parent = target.parent().unwrap_or_else(|| Path::new("."));
            fs::create_dir_all(parent)?;
            let file_name = target.file_name().ok_or_else(|| {
                std::io::Error::new(ErrorKind::InvalidInput, "lock target has no file name")
            })?;
            let lock_path = parent.join(format!(".{}.lock", file_name.to_string_lossy()));
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(lock_path)?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self { _file: file })
        }
        #[cfg(not(unix))]
        {
            let _ = target;
            Ok(Self {})
        }
    }
}
