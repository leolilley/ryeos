//! Daemon boot/exit marker + startup disk-space check.
//!
//! The daemon writes a `running` marker at process start — before its
//! control socket exists — and an `exited` marker on any handled shutdown.
//! Two consumers:
//!
//! - The lifecycle status probe reads it to tell a *booting* daemon (live
//!   pid, no socket yet) apart from a stopped one.
//! - A `SIGKILL` or hard crash cannot write an exit marker — so on the next
//!   startup, a marker still in the `running` state whose pid is no longer
//!   alive is reported as an unclean exit (inferred crash). This turns "the
//!   daemon silently died" into a visible signal on the next `start`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

const MARKER_FILE: &str = "lifecycle.json";

/// Warn when the state filesystem has less free space than this (bytes).
const LOW_DISK_THRESHOLD_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LifecycleMarker {
    /// The daemon is (or was) up. If the pid is dead and no `Exited` marker
    /// followed, the run ended uncleanly.
    Running { pid: u32, started_at: String },
    /// The daemon shut down via a handled path, recording why.
    Exited {
        reason: String,
        pid: u32,
        exited_at: String,
    },
}

fn marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join(MARKER_FILE)
}

pub fn read(state_dir: &Path) -> Option<LifecycleMarker> {
    let raw = std::fs::read_to_string(marker_path(state_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Wall-clock age of the current marker file. The running marker is written
/// immediately before startup listener publication, so this bounds how long a
/// live marker may reasonably be treated as the narrow pre-control bootstrap
/// window. A backwards clock jump yields no age rather than a false timeout.
pub fn age(state_dir: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(marker_path(state_dir))
        .ok()?
        .modified()
        .ok()?;
    SystemTime::now().duration_since(modified).ok()
}

fn write(state_dir: &Path, marker: &LifecycleMarker) {
    let result = std::fs::create_dir_all(state_dir).and_then(|_| {
        let body = serde_json::to_string(marker).unwrap_or_default();
        std::fs::write(marker_path(state_dir), body)
    });
    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to write daemon lifecycle marker");
    }
}

/// Record that this process is now serving. Call once at startup, AFTER
/// [`report_previous_exit`] has inspected the prior run's marker.
pub fn record_running(state_dir: &Path) {
    write(
        state_dir,
        &LifecycleMarker::Running {
            pid: std::process::id(),
            started_at: lillux::time::iso8601_now(),
        },
    );
}

/// Record a clean/handled shutdown with its `reason` (e.g. `"signal"`).
pub fn record_exit(state_dir: &Path, reason: &str) {
    write(
        state_dir,
        &LifecycleMarker::Exited {
            reason: reason.to_string(),
            pid: std::process::id(),
            exited_at: lillux::time::iso8601_now(),
        },
    );
}

/// Inspect the previous run's marker and log its outcome: a recorded clean
/// exit, or an inferred unclean exit when the marker is still `running` but the
/// pid is gone.
pub fn report_previous_exit(state_dir: &Path) {
    match read(state_dir) {
        Some(LifecycleMarker::Exited {
            reason, exited_at, ..
        }) => {
            tracing::info!(reason = %reason, at = %exited_at, "previous ryeosd run exited cleanly");
        }
        Some(LifecycleMarker::Running { pid, started_at }) => {
            if process_alive(pid) {
                tracing::warn!(
                    pid,
                    "lifecycle marker shows a running ryeosd (pid {pid}) — another instance may be active"
                );
            } else {
                tracing::warn!(
                    pid,
                    started_at = %started_at,
                    "previous ryeosd run did not shut down cleanly (no exit marker; pid {pid} is gone) — a crash, SIGKILL, or failed startup"
                );
            }
        }
        None => {}
    }
}

/// Warn if the state filesystem is low on free space. Best-effort: a failed
/// probe is silently ignored (never blocks startup).
pub fn check_disk_space(state_dir: &Path) {
    if let Some(free) = available_bytes(state_dir) {
        if free < LOW_DISK_THRESHOLD_BYTES {
            tracing::warn!(
                free_mib = free / (1024 * 1024),
                threshold_mib = LOW_DISK_THRESHOLD_BYTES / (1024 * 1024),
                path = %state_dir.display(),
                "low free disk space on the state filesystem — runtime writes (events, CAS, traces) may fail"
            );
        }
    }
}

#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    // signal 0 performs error checking without sending a signal: 0 ⇒ the
    // process exists and we may signal it.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
#[cfg(not(unix))]
pub fn process_alive(_pid: u32) -> bool {
    false
}

/// Whether the marker's pid is alive AND still a `ryeosd`. A crash leaves a
/// `running` marker behind, and the OS may recycle its pid onto an unrelated
/// process — classifying that as a live daemon would block `ryeos start`
/// indefinitely. Where the process name can't be inspected (no `/proc`),
/// liveness alone decides. (`stop` has its own fail-closed variant of this
/// check with per-reason errors; this one only classifies.)
pub fn process_alive_as_ryeosd(pid: u32) -> bool {
    if !process_alive(pid) {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(comm) => comm.trim() == "ryeosd",
        Err(_) => true,
    }
}

#[cfg(unix)]
fn available_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` writes into a zeroed struct; we only read fields after
    // a success return.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    // Free space available to a non-root caller.
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}
#[cfg(not(unix))]
fn available_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_exit_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        record_running(tmp.path());
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Running { .. })
        ));
        record_exit(tmp.path(), "signal");
        match read(tmp.path()) {
            Some(LifecycleMarker::Exited { reason, .. }) => assert_eq!(reason, "signal"),
            other => panic!("expected exited marker, got {other:?}"),
        }
    }

    #[test]
    fn running_marker_with_dead_pid_is_detectable() {
        // A pid that is essentially never alive in the test isolation.
        let tmp = tempfile::tempdir().unwrap();
        let marker = LifecycleMarker::Running {
            pid: u32::MAX - 1,
            started_at: "2026-01-01T00:00:00Z".into(),
        };
        write(tmp.path(), &marker);
        // The reporter must not panic and must classify it as not-alive.
        assert!(!process_alive(u32::MAX - 1));
        report_previous_exit(tmp.path()); // smoke: logs the inferred-crash path
    }

    #[test]
    fn disk_check_is_best_effort_and_never_panics() {
        let tmp = tempfile::tempdir().unwrap();
        check_disk_space(tmp.path());
        // A non-existent path must not panic; the probe is best-effort,
        // so the value itself is platform-dependent and unasserted.
        let _ = available_bytes(&tmp.path().join("does-not-exist"));
    }
}
