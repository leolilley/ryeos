//! Local host advisory GC lock using flock().
//!
//! Only one GC process should run at a time per CAS root. Uses an
//! exclusive advisory file lock via `flock()` for atomic acquisition —
//! no TOCTOU race. A JSON sidecar file provides observability (who
//! holds it, which phase).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockState {
    node_id: String,
    phase: String,
    acquired_at: String,
    pid: u32,
}

fn lock_path(cas_root: &Path) -> PathBuf {
    cas_root.join("gc.lock")
}

fn state_path(cas_root: &Path) -> PathBuf {
    cas_root.join("gc.state.json")
}

/// RAII guard that holds the flock and cleans up on drop.
pub struct GcLock {
    lock_file: fs::File,
    state_path: PathBuf,
}

impl GcLock {
    /// Acquire an exclusive advisory lock via `flock()`.
    /// Blocks until the lock is available. Stale state from dead
    /// holders is overwritten.
    pub fn acquire(cas_root: &Path, node_id: &str) -> Result<Self> {
        let path = lock_path(cas_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;

        acquire_flock(&file)?;

        let state = LockState {
            node_id: node_id.to_string(),
            phase: "init".to_string(),
            acquired_at: chrono::Utc::now().to_rfc3339(),
            pid: std::process::id(),
        };
        let sp = state_path(cas_root);
        let tmp = sp.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&state)?)?;
        fs::rename(&tmp, &sp)?;

        tracing::debug!(node_id, "acquired GC lock");
        Ok(Self {
            lock_file: file,
            state_path: sp,
        })
    }

    /// Update the current GC phase in the state file.
    pub fn update_phase(&self, node_id: &str, phase: &str) -> Result<()> {
        let state = LockState {
            node_id: node_id.to_string(),
            phase: phase.to_string(),
            acquired_at: chrono::Utc::now().to_rfc3339(),
            pid: std::process::id(),
        };
        let tmp = self.state_path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&state)?)?;
        fs::rename(&tmp, &self.state_path)?;
        Ok(())
    }
}

impl Drop for GcLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.state_path);
        let _ = release_flock(&self.lock_file);
    }
}

#[cfg(unix)]
fn acquire_flock(file: &fs::File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if ret != 0 {
        bail!("flock(LOCK_EX) failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn release_flock(file: &fs::File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_UN) };
    if ret != 0 {
        bail!("flock(LOCK_UN) failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn acquire_flock(file: &fs::File) -> Result<()> {
    let _ = file;
    Ok(())
}

#[cfg(not(unix))]
fn release_flock(file: &fs::File) -> Result<()> {
    let _ = file;
    Ok(())
}
