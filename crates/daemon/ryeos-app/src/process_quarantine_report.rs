//! Operator report of process quarantines.
//!
//! The runtime row (`thread_runtime.process_quarantine`) is the authority.
//! `ryeos node doctor` runs offline and never opens the runtime database, so
//! the daemon mirrors the quarantined rows into this report after each active
//! recovery pass, and removes it when none remain. The report carries no
//! authority; it only lets the doctor name what recovery is holding.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::runtime_db::QuarantinedProcess;

pub const PROCESS_QUARANTINE_REPORT_FILE: &str = "process-quarantine.json";
const MAX_PROCESS_QUARANTINE_REPORT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessQuarantineReport {
    pub written_at: String,
    pub threads: Vec<QuarantinedProcess>,
}

/// Where the report of the node whose runtime state directory is
/// `state_dir` lives.
pub fn report_path(state_dir: &Path) -> PathBuf {
    state_dir.join(PROCESS_QUARANTINE_REPORT_FILE)
}

/// Mirror the current quarantines. An empty set removes the report.
pub fn write_report(state_dir: &Path, threads: Vec<QuarantinedProcess>) -> Result<()> {
    let path = report_path(state_dir);
    if threads.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("remove process quarantine report {}", path.display())),
        };
    }
    let report = ProcessQuarantineReport {
        written_at: lillux::time::iso8601_now(),
        threads,
    };
    let bytes = serde_json::to_vec_pretty(&report)?;
    anyhow::ensure!(
        u64::try_from(bytes.len())? <= MAX_PROCESS_QUARANTINE_REPORT_BYTES,
        "process quarantine report exceeds its bound"
    );
    lillux::atomic_fs::atomic_write(&path, &bytes)
        .with_context(|| format!("write process quarantine report {}", path.display()))
}

/// Read the last mirrored report. `None` when the daemon recorded none.
pub fn read_report(state_dir: &Path) -> Result<Option<ProcessQuarantineReport>> {
    let Some(directory) = lillux::PinnedDirectory::open(state_dir)? else {
        return Ok(None);
    };
    let Some(record) = directory
        .open_pinned_regular(std::ffi::OsStr::new(PROCESS_QUARANTINE_REPORT_FILE), false)?
    else {
        return Ok(None);
    };
    let observation = record.observation()?;
    let raw = record.read_stable_bounded(&observation, MAX_PROCESS_QUARANTINE_REPORT_BYTES)?;
    serde_json::from_slice(&raw)
        .map(Some)
        .context("process quarantine report is unreadable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_db::{ProcessQuarantine, ProcessQuarantineReason};

    #[test]
    fn report_round_trips_and_an_empty_set_removes_it() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_report(dir.path()).unwrap().is_none());
        let threads = vec![QuarantinedProcess {
            thread_id: "T-q".to_string(),
            pgid: Some(4242),
            quarantine: ProcessQuarantine {
                reason: ProcessQuarantineReason::SameBootGroupPresent,
                first_seen_at_ms: 1,
                last_checked_at_ms: 2,
            },
        }];
        write_report(dir.path(), threads.clone()).unwrap();
        assert_eq!(read_report(dir.path()).unwrap().unwrap().threads, threads);
        write_report(dir.path(), Vec::new()).unwrap();
        assert!(read_report(dir.path()).unwrap().is_none());
        write_report(dir.path(), Vec::new()).unwrap();
    }
}
