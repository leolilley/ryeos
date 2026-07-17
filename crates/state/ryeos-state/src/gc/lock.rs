//! GC lock — advisory file locking via flock().
//!
//! Prevents concurrent `ryeos gc` invocations from corrupting state.
//! Uses RAII: lock acquired on creation, released on drop.
//!
//! Lock file at `runtime_state_dir/gc.lock`.
//! State file at `runtime_state_dir/gc.state.json` for observability.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::json;

/// RAII guard for the GC lock.
///
/// Acquires an exclusive flock on `runtime_state_dir/gc.lock`. The lock is
/// released when this guard is dropped (or the process exits).
pub struct GcLock {
    _lock_file: File,
    directory: lillux::PinnedDirectory,
    state_file: Option<File>,
}

impl GcLock {
    /// Establish the persistent GC lock anchor during ordinary mutable node
    /// initialization. A later dry-run can then serialize with GC without
    /// creating files merely by inspecting state.
    pub fn ensure_anchor(runtime_state_dir: &Path) -> Result<()> {
        let directory = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
            .context("open runtime state directory for GC lock initialization")?;
        directory
            .open_regular_create(std::ffi::OsStr::new("gc.lock"), true, false, 0o600)
            .context("establish GC lock anchor")?;
        directory.sync().context("sync GC lock anchor directory")
    }

    /// Acquire the GC lock. Blocks until acquired or times out.
    ///
    /// Creates a JSON sidecar file at `runtime_state_dir/gc.state.json`
    /// for observability (who holds the lock, current phase, PID).
    pub fn acquire(runtime_state_dir: &Path, node_id: &str) -> Result<Self> {
        let lock_path = runtime_state_dir.join("gc.lock");
        let directory = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
            .context("open runtime state directory for GC lock")?;
        let lock_file = directory
            .open_regular_create(std::ffi::OsStr::new("gc.lock"), true, false, 0o600)
            .with_context(|| format!("failed to open GC lock file: {}", lock_path.display()))?;

        // Non-blocking exclusive lock
        if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!(
                "failed to acquire GC lock at {}: {} (another GC run may be in progress)",
                lock_path.display(),
                err
            );
        }

        // Write state sidecar
        let pid = std::process::id();
        let state = json!({
            "pid": pid,
            "node_id": node_id,
            "phase": "acquired",
            "started_at": lillux::time::iso8601_now(),
        });
        let state_name = std::ffi::OsStr::new("gc.state.json");
        let existing_state = directory
            .open_regular(state_name, true)
            .context("open existing GC state file")?;
        let state_bytes = serde_json::to_vec_pretty(&state)?;
        directory
            .atomic_write_if_same(state_name, existing_state.as_ref(), &state_bytes, 0o600)
            .context("publish GC state file")?;
        let state_file = directory
            .open_regular(state_name, true)?
            .ok_or_else(|| anyhow::anyhow!("published GC state file disappeared"))?;

        tracing::info!(pid = pid, node_id = node_id, "GC lock acquired");

        Ok(Self {
            _lock_file: lock_file,
            directory,
            state_file: Some(state_file),
        })
    }

    /// Acquire the established GC lock without creating the anchor, a state
    /// sidecar, or any directory. This is the literal mutation-free dry-run
    /// path; absence is a current-format initialization error.
    pub fn acquire_existing(runtime_state_dir: &Path) -> Result<Self> {
        let lock_path = runtime_state_dir.join("gc.lock");
        let directory = lillux::PinnedDirectory::open(runtime_state_dir)?
            .ok_or_else(|| anyhow::anyhow!("runtime state directory is absent"))?;
        let lock_file = directory
            .open_regular(std::ffi::OsStr::new("gc.lock"), true)?
            .ok_or_else(|| anyhow::anyhow!("GC lock anchor is absent: {}", lock_path.display()))?;
        if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!(
                "failed to acquire GC lock at {}: {} (another GC run may be in progress)",
                lock_path.display(),
                err
            );
        }
        Ok(Self {
            _lock_file: lock_file,
            directory,
            state_file: None,
        })
    }

    /// Update the current phase in the state sidecar.
    pub fn update_phase(&self, phase: &str) -> Result<()> {
        let Some(state_file) = self.state_file.as_ref() else {
            return Ok(());
        };
        let mut file = state_file.try_clone().context("clone GC state file")?;
        file.seek(SeekFrom::Start(0))?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        if let Ok(mut state) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(obj) = state.as_object_mut() {
                obj.insert("phase".to_string(), json!(phase));
                let bytes = serde_json::to_vec_pretty(&state)?;
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
        }
        Ok(())
    }
}

impl Drop for GcLock {
    fn drop(&mut self) {
        // Remove state file BEFORE unlocking to prevent race:
        // If we unlock first, another process could acquire the lock and
        // write its own sidecar — then our remove_file would delete the
        // NEW holder's state file.
        if let Some(state_file) = self.state_file.take() {
            let _ = self
                .directory
                .remove_if_same(std::ffi::OsStr::new("gc.state.json"), &state_file);
        }

        // Release flock (implicit on close, but explicit is cleaner)
        unsafe {
            libc::flock(self._lock_file.as_raw_fd(), libc::LOCK_UN);
        }

        tracing::info!("GC lock released");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn existing_lock_path_is_mutation_free() {
        let tmp = TempDir::new().unwrap();
        let runtime_state_dir = tmp.path().join("state");
        fs::create_dir_all(&runtime_state_dir).unwrap();
        assert!(GcLock::acquire_existing(&runtime_state_dir).is_err());
        assert!(!runtime_state_dir.join("gc.lock").exists());

        GcLock::ensure_anchor(&runtime_state_dir).unwrap();
        let before = fs::read_dir(&runtime_state_dir).unwrap().count();
        drop(GcLock::acquire_existing(&runtime_state_dir).unwrap());
        assert_eq!(fs::read_dir(&runtime_state_dir).unwrap().count(), before);
        assert!(!runtime_state_dir.join("gc.state.json").exists());
    }

    #[test]
    fn lock_acquire_and_release() {
        let tmp = TempDir::new().unwrap();
        let runtime_state_dir = tmp.path().join("state");
        fs::create_dir_all(&runtime_state_dir).unwrap();

        {
            let _lock = GcLock::acquire(&runtime_state_dir, "test-node").unwrap();
            assert!(runtime_state_dir.join("gc.lock").exists());
            assert!(runtime_state_dir.join("gc.state.json").exists());
        }

        // After drop, state file is cleaned up
        assert!(!runtime_state_dir.join("gc.state.json").exists());
        // Lock file stays (it's the persistent lock anchor)
    }

    #[test]
    fn lock_concurrent_fails() {
        let tmp = TempDir::new().unwrap();
        let runtime_state_dir = tmp.path().join("state");
        fs::create_dir_all(&runtime_state_dir).unwrap();

        let _lock1 = GcLock::acquire(&runtime_state_dir, "node-1").unwrap();

        let result = GcLock::acquire(&runtime_state_dir, "node-2");
        let error = match result {
            Ok(_) => panic!("concurrent GC lock acquisition must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("failed to acquire GC lock"));
    }

    #[test]
    fn lock_update_phase() {
        let tmp = TempDir::new().unwrap();
        let runtime_state_dir = tmp.path().join("state");
        fs::create_dir_all(&runtime_state_dir).unwrap();

        let lock = GcLock::acquire(&runtime_state_dir, "test-node").unwrap();
        lock.update_phase("compact").unwrap();

        let content = fs::read_to_string(runtime_state_dir.join("gc.state.json")).unwrap();
        assert!(content.contains("\"compact\""));
    }
}
