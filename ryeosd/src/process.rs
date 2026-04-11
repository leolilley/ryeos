use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

pub fn remove_stale_socket(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
    }
    Ok(())
}

/// Check if a process group is alive.
pub fn pgid_alive(pgid: i64) -> bool {
    // kill(0, -pgid) checks if any process in the group exists
    unsafe { libc::kill(-(pgid as i32), 0) == 0 }
}

/// Return the daemon's own process group ID.
pub fn daemon_pgid() -> i64 {
    unsafe { libc::getpgid(0) as i64 }
}

/// Return true if the given PGID matches the daemon's own process group.
/// Killing such a PGID would kill the daemon itself.
pub fn is_daemon_pgid(pgid: i64) -> bool {
    pgid == daemon_pgid()
}

/// Send SIGTERM to a process group, wait for grace period, then SIGKILL if needed.
pub fn kill_process_group(pgid: i64, grace: Duration) -> KillResult {
    let neg_pgid = -(pgid as i32);

    // SIGTERM the entire process group
    let term_result = unsafe { libc::kill(neg_pgid, libc::SIGTERM) };
    if term_result != 0 {
        let errno = std::io::Error::last_os_error();
        if errno.raw_os_error() == Some(libc::ESRCH) {
            return KillResult {
                success: true,
                method: "already_dead",
            };
        }
        return KillResult {
            success: false,
            method: "sigterm_failed",
        };
    }

    // Poll for death within grace period
    let deadline = std::time::Instant::now() + grace;
    let poll_interval = Duration::from_millis(100);
    while std::time::Instant::now() < deadline {
        if !pgid_alive(pgid) {
            return KillResult {
                success: true,
                method: "terminated",
            };
        }
        std::thread::sleep(poll_interval);
    }

    // Grace period expired — SIGKILL
    unsafe {
        libc::kill(neg_pgid, libc::SIGKILL);
    }

    // Brief wait for SIGKILL to take effect
    std::thread::sleep(Duration::from_millis(200));

    KillResult {
        success: !pgid_alive(pgid),
        method: "killed",
    }
}

pub struct KillResult {
    pub success: bool,
    pub method: &'static str,
}
