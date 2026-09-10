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

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const MARKER_FILE: &str = "lifecycle.json";
const MAX_MARKER_BYTES: u64 = 64 * 1024;

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
        started_at: String,
        exited_at: String,
        error: Option<String>,
    },
}

pub fn read(state_dir: &Path) -> Option<LifecycleMarker> {
    let directory = lillux::PinnedDirectory::open(state_dir).ok()??;
    let marker = directory
        .open_pinned_regular(std::ffi::OsStr::new(MARKER_FILE), false)
        .ok()??;
    let observation = marker.observation().ok()?;
    let raw = marker
        .read_stable_bounded(&observation, MAX_MARKER_BYTES)
        .ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Wall-clock age of the current marker file. The running marker is written
/// immediately before startup listener publication, so this bounds how long a
/// live marker may reasonably be treated as the narrow pre-control bootstrap
/// window. A backwards clock jump yields no age rather than a false timeout.
pub fn age(state_dir: &Path) -> Option<Duration> {
    let directory = lillux::PinnedDirectory::open(state_dir).ok()??;
    let marker = directory
        .open_pinned_regular(std::ffi::OsStr::new(MARKER_FILE), false)
        .ok()??;
    marker.modification_age().ok()?
}

fn write(state_dir: &Path, marker: &LifecycleMarker) {
    let result = (|| -> anyhow::Result<()> {
        let directory = lillux::PinnedDirectory::open(state_dir)?
            .ok_or_else(|| anyhow::anyhow!("lifecycle state directory is absent"))?;
        let existing = directory.open_pinned_regular(std::ffi::OsStr::new(MARKER_FILE), false)?;
        let body = serde_json::to_vec(marker)?;
        directory.atomic_write_pinned_if_same(
            std::ffi::OsStr::new(MARKER_FILE),
            existing.as_ref(),
            &body,
            0o600,
        )?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!(error = %e, "failed to write daemon lifecycle marker");
    }
}

/// Record this exact daemon startup attempt. Call after the state lock is held
/// and after [`report_previous_exit`] has inspected the prior run's marker;
/// readiness remains the lifecycle protocol's separate authority.
pub fn record_running(state_dir: &Path) -> String {
    let started_at = lillux::time::iso8601_now();
    write(
        state_dir,
        &LifecycleMarker::Running {
            pid: std::process::id(),
            started_at: started_at.clone(),
        },
    );
    started_at
}

/// Record a clean/handled shutdown with its `reason` (e.g. `"signal"`).
pub fn record_exit(state_dir: &Path, reason: &str, started_at: &str, error: Option<&str>) {
    const MAX_ERROR_BYTES: usize = 8 * 1024;
    let error = error.map(|value| {
        let mut end = value.len().min(MAX_ERROR_BYTES);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value[..end].trim().to_owned()
    });
    write(
        state_dir,
        &LifecycleMarker::Exited {
            reason: reason.to_string(),
            pid: std::process::id(),
            started_at: started_at.to_owned(),
            exited_at: lillux::time::iso8601_now(),
            error,
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
    if let Some(free) = available_bytes(state_dir)
        && free < LOW_DISK_THRESHOLD_BYTES
    {
        tracing::warn!(
            free_mib = free / (1024 * 1024),
            threshold_mib = LOW_DISK_THRESHOLD_BYTES / (1024 * 1024),
            path = %state_dir.display(),
            "low free disk space on the state filesystem — runtime writes (events, CAS, traces) may fail"
        );
    }
}

pub fn process_alive(pid: u32) -> bool {
    lillux::diagnostic_process_is_live(pid)
}

/// Whether the marker's pid is alive AND still a `ryeosd`. A crash leaves a
/// `running` marker behind, and the OS may recycle its pid onto an unrelated
/// process — classifying that as a live daemon would block `ryeos start`
/// indefinitely. Where the process name can't be inspected (no `/proc`),
/// liveness alone decides. (`stop` has its own fail-closed variant of this
/// check with per-reason errors; this one only classifies.)
pub fn process_alive_as_ryeosd(pid: u32) -> bool {
    lillux::diagnostic_process_matches_executable_name(pid, std::ffi::OsStr::new("ryeosd"))
}

fn available_bytes(path: &Path) -> Option<u64> {
    lillux::PinnedDirectory::open(path)
        .ok()??
        .filesystem_capacity()
        .ok()
        .map(|capacity| capacity.available_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_exit_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let started_at = record_running(tmp.path());
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Running { .. })
        ));
        record_exit(
            tmp.path(),
            "startup_failed",
            &started_at,
            Some("failed to bind 127.0.0.1:7400"),
        );
        match read(tmp.path()) {
            Some(LifecycleMarker::Exited {
                reason,
                started_at: retained_start,
                error,
                ..
            }) => {
                assert_eq!(reason, "startup_failed");
                assert_eq!(retained_start, started_at);
                assert_eq!(error.as_deref(), Some("failed to bind 127.0.0.1:7400"));
            }
            other => panic!("expected exited marker, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_marker_never_follows_a_replaced_entry() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, b"retained").unwrap();
        symlink(&victim, tmp.path().join(MARKER_FILE)).unwrap();
        let _ = record_running(tmp.path());
        assert_eq!(std::fs::read(victim).unwrap(), b"retained");
        assert!(read(tmp.path()).is_none());
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
