//! GC lock — advisory file locking via flock().
//!
//! Prevents concurrent `rye-gc` invocations from corrupting state.
//! Uses RAII: lock acquired on creation, released on drop.
//!
//! Lock file at `state_root/gc.lock`.
//! State file at `state_root/gc.state.json` for observability.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::json;

/// RAII guard for the GC lock.
///
/// Acquires an exclusive flock on `state_root/gc.lock`. The lock is
/// released when this guard is dropped (or the process exits).
#[derive(Debug)]
pub struct GcLock {
    _lock_file: File,
    state_path: std::path::PathBuf,
}

impl GcLock {
    /// Acquire the GC lock. Blocks until acquired or times out.
    ///
    /// Creates a JSON sidecar file at `state_root/gc.state.json`
    /// for observability (who holds the lock, current phase, PID).
    pub fn acquire(state_root: &Path, node_id: &str) -> Result<Self> {
        let lock_path = state_root.join("gc.lock");
        let state_path = state_root.join("gc.state.json");

        // Ensure state_root exists
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .context("failed to create state_root for GC lock")?;
        }

        // Open (create if needed) and lock
        let lock_file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)
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
        fs::write(&state_path, serde_json::to_string_pretty(&state)?)
            .context("failed to write GC state file")?;

        tracing::info!(
            pid = pid,
            node_id = node_id,
            "GC lock acquired"
        );

        Ok(Self {
            _lock_file: lock_file,
            state_path,
        })
    }

    /// Update the current phase in the state sidecar.
    pub fn update_phase(&self, phase: &str) -> Result<()> {
        if let Ok(content) = fs::read_to_string(&self.state_path) {
            if let Ok(mut state) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(obj) = state.as_object_mut() {
                    obj.insert("phase".to_string(), json!(phase));
                    let _ = fs::write(&self.state_path, serde_json::to_string_pretty(&state)?);
                }
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
        let _ = fs::remove_file(&self.state_path);

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
    use tempfile::TempDir;

    #[test]
    fn lock_acquire_and_release() {
        let tmp = TempDir::new().unwrap();
        let state_root = tmp.path().join("state");
        fs::create_dir_all(&state_root).unwrap();

        {
            let _lock = GcLock::acquire(&state_root, "test-node").unwrap();
            assert!(state_root.join("gc.lock").exists());
            assert!(state_root.join("gc.state.json").exists());
        }

        // After drop, state file is cleaned up
        assert!(!state_root.join("gc.state.json").exists());
        // Lock file stays (it's the persistent lock anchor)
    }

    #[test]
    fn lock_concurrent_fails() {
        let tmp = TempDir::new().unwrap();
        let state_root = tmp.path().join("state");
        fs::create_dir_all(&state_root).unwrap();

        let _lock1 = GcLock::acquire(&state_root, "node-1").unwrap();

        let result = GcLock::acquire(&state_root, "node-2");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed to acquire GC lock"));
    }

    #[test]
    fn lock_update_phase() {
        let tmp = TempDir::new().unwrap();
        let state_root = tmp.path().join("state");
        fs::create_dir_all(&state_root).unwrap();

        let lock = GcLock::acquire(&state_root, "test-node").unwrap();
        lock.update_phase("compact").unwrap();

        let content = fs::read_to_string(state_root.join("gc.state.json")).unwrap();
        assert!(content.contains("\"compact\""));
    }
}
