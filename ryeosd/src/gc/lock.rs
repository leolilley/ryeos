//! File-based distributed GC lock.
//!
//! Only one GC process should run at a time per CAS root. The lock file
//! contains the node ID and phase for progress tracking.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// GC lock state written to the lock file.
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

/// Acquire the GC lock. Fails if already held by another process.
///
/// Stale locks (from dead processes) are automatically broken.
pub fn acquire(cas_root: &Path, node_id: &str) -> Result<()> {
    let path = lock_path(cas_root);

    // Check for existing lock
    if path.exists() {
        if let Ok(data) = fs::read_to_string(&path) {
            if let Ok(state) = serde_json::from_str::<LockState>(&data) {
                // Check if the holding process is still alive
                if is_process_alive(state.pid) {
                    bail!(
                        "GC lock held by node {} (pid {}, phase: {})",
                        state.node_id,
                        state.pid,
                        state.phase
                    );
                }
                // Stale lock — break it
                tracing::warn!(
                    old_node = %state.node_id,
                    old_pid = state.pid,
                    "breaking stale GC lock"
                );
            }
        }
    }

    let state = LockState {
        node_id: node_id.to_string(),
        phase: "init".to_string(),
        acquired_at: chrono::Utc::now().to_rfc3339(),
        pid: std::process::id(),
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&state)?)?;
    fs::rename(&tmp, &path)?;

    tracing::debug!(node_id, "acquired GC lock");
    Ok(())
}

/// Release the GC lock.
pub fn release(cas_root: &Path, _node_id: &str) -> Result<()> {
    let path = lock_path(cas_root);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Update the current GC phase in the lock file.
pub fn update_phase(cas_root: &Path, node_id: &str, phase: &str) -> Result<()> {
    let path = lock_path(cas_root);
    if !path.exists() {
        return Ok(());
    }

    let state = LockState {
        node_id: node_id.to_string(),
        phase: phase.to_string(),
        acquired_at: chrono::Utc::now().to_rfc3339(),
        pid: std::process::id(),
    };

    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&state)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Check if a process is alive (Unix-specific).
fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Signal 0 checks existence without sending a signal
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false // Conservative: assume dead on non-Unix
    }
}
