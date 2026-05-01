//! Operator state lock for mutual exclusion between daemon and standalone mode.
//!
//! The daemon acquires an exclusive lock on `<state_dir>/.ai/state/operator.lock`
//! at startup and holds it for its lifetime. Standalone state-backed services
//! must acquire the same lock or fail with "daemon is running."
//!
//! Uses `flock(LOCK_EX | LOCK_NB)` for non-blocking exclusive access. The lock
//! is automatically released when the file descriptor is closed (process exit,
//! including panic).

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// RAII guard for the operator state lock.
///
/// Holds the lock file open for the lifetime of the guard. Drop releases.
pub struct StateLock {
    _file: File,
}

impl std::fmt::Debug for StateLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateLock").finish_non_exhaustive()
    }
}

impl StateLock {
    /// Attempt to acquire an exclusive, non-blocking lock on `lock_path`.
    ///
    /// Creates the file and parent directories if they don't exist.
    /// Returns `Ok(StateLock)` if the lock was acquired.
    /// Returns an error if another process holds the lock.
    pub fn acquire(lock_path: &Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("create state lock directory {}", parent.display())
            })?;
        }

        let file = File::create(lock_path).with_context(|| {
            format!("create state lock file {}", lock_path.display())
        })?;

        // Write our PID for diagnostics (who holds the lock)
        #[cfg(unix)]
        {
            use std::io::Write;
            let _ = writeln!(&file, "{}", std::process::id());
        }

        // Non-blocking exclusive lock
        match flock_exclusive_nb(&file) {
            Ok(()) => Ok(StateLock {
                _file: file,
            }),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Another process holds the lock. Try to read its PID.
                let holder_pid = fs::read_to_string(lock_path)
                    .ok()
                    .and_then(|s| s.trim().to_string().into())
                    .unwrap_or_else(|| "unknown".to_string());
                bail!(
                    "state lock held by another process (pid: {}); stop the daemon or other standalone service before proceeding",
                    holder_pid
                );
            }
            Err(e) => Err(e).with_context(|| {
                format!("acquire state lock {}", lock_path.display())
            }),
        }
    }
}

#[cfg(unix)]
fn flock_exclusive_nb(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if result == -1 {
        let err = io::Error::last_os_error();
        // Map EWOULDBLOCK to WouldBlock kind
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, err));
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(not(unix))]
fn flock_exclusive_nb(file: &File) -> io::Result<()> {
    // Non-Unix: no-op (no flock). This module is Unix-only in practice.
    Ok(())
}

/// Return the default lock path for a given state directory.
pub fn default_lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join(".ai").join("state").join("operator.lock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_and_release_lock() {
        let tmpdir = TempDir::new().unwrap();
        let lock_path = tmpdir.path().join("test.lock");

        {
            let _lock = StateLock::acquire(&lock_path).unwrap();
            assert!(lock_path.exists());
            // Lock released when dropped
        }

        // Should be able to re-acquire after drop
        let _lock2 = StateLock::acquire(&lock_path).unwrap();
    }

    #[test]
    fn double_acquire_fails() {
        let tmpdir = TempDir::new().unwrap();
        let lock_path = tmpdir.path().join("test.lock");

        let _lock1 = StateLock::acquire(&lock_path).unwrap();

        let result = StateLock::acquire(&lock_path);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("state lock held"),
            "expected 'state lock held' in error, got: {err_msg}"
        );
    }

    #[test]
    fn default_lock_path_is_under_state() {
        let path = default_lock_path(Path::new("/var/lib/ryeosd"));
        assert_eq!(path, PathBuf::from("/var/lib/ryeosd/.ai/state/operator.lock"));
    }

    #[test]
    fn lock_creates_parent_dirs() {
        let tmpdir = TempDir::new().unwrap();
        let lock_path = tmpdir.path().join("nested").join("dir").join("test.lock");

        let _lock = StateLock::acquire(&lock_path).unwrap();
        assert!(lock_path.exists());
    }
}
