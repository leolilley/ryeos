//! Operator state lock for mutual exclusion between daemon and standalone mode.
//!
//! The daemon acquires an exclusive lock on `<app_root>/.ai/state/operator.lock`
//! at startup and holds it for its lifetime. Standalone state-backed services
//! must acquire the same lock or fail with "daemon is running."
//!
//! Uses `flock(LOCK_EX | LOCK_NB)` for non-blocking exclusive access. The lock
//! is automatically released when the file descriptor is closed (process exit,
//! including panic).

use std::path::{Path, PathBuf};

use anyhow::Result;

/// RAII guard for the operator state lock.
///
/// Holds the lock file open for the lifetime of the guard. Drop releases.
pub struct StateLock {
    inner: lillux::ExactExclusiveFileLock,
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
        Ok(Self {
            inner: lillux::ExactExclusiveFileLock::acquire(lock_path)?,
        })
    }

    /// Acquire the same exact state authority with a bounded wait for kernel
    /// teardown of a crashed predecessor generation.
    pub fn acquire_with_timeout(lock_path: &Path, timeout: std::time::Duration) -> Result<Self> {
        Ok(Self {
            inner: lillux::ExactExclusiveFileLock::acquire_with_timeout(lock_path, timeout)?,
        })
    }

    /// Acquire the already-existing operator lock without creating or writing
    /// any filesystem entry. Read-only inspections use this to prove daemon
    /// exclusion without changing the inspected state namespace.
    pub fn acquire_existing_read_only(lock_path: &Path) -> Result<Self> {
        Ok(Self {
            inner: lillux::ExactExclusiveFileLock::acquire_existing_read_only(lock_path)?,
        })
    }

    /// Require this guard to protect the exact operational lock of `app_root`.
    pub fn ensure_protects_app_root(&self, app_root: &Path) -> Result<()> {
        self.inner.ensure_path_binding(&default_lock_path(app_root))
    }
}

/// Return the default lock path for a given state directory.
pub fn default_lock_path(app_root: &Path) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("operator.lock")
}

#[cfg(test)]
mod tests {
    use std::fs;

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
    fn failed_acquire_preserves_holder_pid() {
        let tmpdir = TempDir::new().unwrap();
        let lock_path = tmpdir.path().join("test.lock");

        let _lock1 = StateLock::acquire(&lock_path).unwrap();
        let holder_pid = fs::read_to_string(&lock_path).unwrap();

        let result = StateLock::acquire(&lock_path);
        assert!(result.is_err());

        let after_failed_acquire = fs::read_to_string(&lock_path).unwrap();
        assert_eq!(after_failed_acquire, holder_pid);
    }

    #[test]
    fn bounded_acquire_never_steals_live_state_authority() {
        let tmpdir = TempDir::new().unwrap();
        let lock_path = tmpdir.path().join("test.lock");
        let _lock1 = StateLock::acquire(&lock_path).unwrap();
        let holder_pid = fs::read_to_string(&lock_path).unwrap();

        let error =
            StateLock::acquire_with_timeout(&lock_path, std::time::Duration::from_millis(75))
                .unwrap_err();
        assert!(
            format!("{error:#}").contains("after waiting 0.1s"),
            "bounded acquisition did not report its refusal: {error:#}"
        );
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), holder_pid);
    }

    #[test]
    fn bounded_acquire_obtains_the_exact_lock_after_holder_release() {
        let tmpdir = TempDir::new().unwrap();
        let lock_path = tmpdir.path().join("test.lock");
        let holder = StateLock::acquire(&lock_path).unwrap();
        let waiter_path = lock_path.clone();
        let waiter = std::thread::spawn(move || {
            StateLock::acquire_with_timeout(&waiter_path, std::time::Duration::from_secs(1))
        });

        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(holder);
        let acquired = waiter
            .join()
            .expect("bounded lock waiter panicked")
            .expect("bounded waiter did not acquire the released exact lock");
        assert!(StateLock::acquire(&lock_path).is_err());
        drop(acquired);
        StateLock::acquire(&lock_path).expect("released bounded lock was not reacquirable");
    }

    #[test]
    fn default_lock_path_is_under_state() {
        let path = default_lock_path(Path::new("/var/lib/ryeosd"));
        assert_eq!(
            path,
            PathBuf::from("/var/lib/ryeosd/.ai/state/operator.lock")
        );
    }

    #[test]
    fn read_only_acquire_preserves_existing_lock_file_and_never_creates_one() {
        let tmpdir = TempDir::new().unwrap();
        let missing = tmpdir.path().join("missing.lock");
        assert!(StateLock::acquire_existing_read_only(&missing).is_err());
        assert!(!missing.exists());

        let lock_path = tmpdir.path().join("existing.lock");
        fs::write(&lock_path, b"retained-holder\n").unwrap();
        let before = fs::metadata(&lock_path).unwrap();
        {
            let _lock = StateLock::acquire_existing_read_only(&lock_path).unwrap();
            assert_eq!(fs::read(&lock_path).unwrap(), b"retained-holder\n");
        }
        let after = fs::metadata(&lock_path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(fs::read(&lock_path).unwrap(), b"retained-holder\n");
    }

    #[test]
    fn lock_creates_parent_dirs() {
        let tmpdir = TempDir::new().unwrap();
        let lock_path = tmpdir.path().join("nested").join("dir").join("test.lock");

        let _lock = StateLock::acquire(&lock_path).unwrap();
        assert!(lock_path.exists());
    }
}
