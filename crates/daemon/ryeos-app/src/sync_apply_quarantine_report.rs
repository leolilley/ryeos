//! Operator report of quarantined project snapshot applies.
//!
//! Startup recovery quarantines an interrupted apply it can never resolve:
//! the job row is the authority (state `failed`, phase `quarantined`), and the
//! apply's transaction directory is kept untouched for inspection.
//! `ryeos node doctor` runs offline and never opens the state database, so
//! the daemon mirrors the quarantined jobs into this report after startup
//! recovery and removes it when none remain. The report carries no authority;
//! it only lets the doctor name what recovery is holding.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const SYNC_APPLY_QUARANTINE_REPORT_FILE: &str = "sync-apply-quarantine.json";
const MAX_SYNC_APPLY_QUARANTINE_REPORT_BYTES: u64 = 1024 * 1024;
/// Entries beyond this many are counted in `omitted` instead of listed.
pub const MAX_SYNC_APPLY_QUARANTINE_ENTRIES: usize = 256;
/// Longest `detail` kept per entry, in characters.
pub const MAX_SYNC_APPLY_QUARANTINE_DETAIL_CHARS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncApplyQuarantineEntry {
    pub job_id: String,
    pub code: String,
    pub detail: String,
    pub quarantined_at: String,
    /// `None` when the job's operation could not be decoded.
    pub project_path: Option<String>,
    /// `None` when the job's operation could not be decoded.
    pub transaction_id: Option<String>,
    /// Full path of the kept transaction directory; `None` when the
    /// operation could not be decoded and no path is known.
    pub transaction_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncApplyQuarantineReport {
    pub written_at: String,
    pub jobs: Vec<SyncApplyQuarantineEntry>,
    pub omitted: u64,
}

/// Where the report of the node whose runtime state directory is
/// `state_dir` lives.
pub fn report_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SYNC_APPLY_QUARANTINE_REPORT_FILE)
}

/// Mirror the current quarantines. An empty set removes the report. The
/// listing is bounded: entries past the cap are counted in `omitted`.
pub fn write_report(state_dir: &Path, mut jobs: Vec<SyncApplyQuarantineEntry>) -> Result<()> {
    let path = report_path(state_dir);
    if jobs.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("remove sync apply quarantine report {}", path.display())),
        };
    }
    let omitted = jobs.len().saturating_sub(MAX_SYNC_APPLY_QUARANTINE_ENTRIES);
    jobs.truncate(MAX_SYNC_APPLY_QUARANTINE_ENTRIES);
    for job in &mut jobs {
        if job.detail.chars().count() > MAX_SYNC_APPLY_QUARANTINE_DETAIL_CHARS {
            job.detail = job
                .detail
                .chars()
                .take(MAX_SYNC_APPLY_QUARANTINE_DETAIL_CHARS)
                .collect();
        }
    }
    let report = SyncApplyQuarantineReport {
        written_at: lillux::time::iso8601_now(),
        jobs,
        omitted: u64::try_from(omitted).context("omitted count exceeds u64")?,
    };
    let bytes = serde_json::to_vec_pretty(&report)?;
    anyhow::ensure!(
        u64::try_from(bytes.len())? <= MAX_SYNC_APPLY_QUARANTINE_REPORT_BYTES,
        "sync apply quarantine report exceeds its bound"
    );
    lillux::atomic_fs::atomic_write(&path, &bytes)
        .with_context(|| format!("write sync apply quarantine report {}", path.display()))
}

/// Read the last mirrored report. `None` when the daemon recorded none.
pub fn read_report(state_dir: &Path) -> Result<Option<SyncApplyQuarantineReport>> {
    let Some(directory) = lillux::PinnedDirectory::open(state_dir)? else {
        return Ok(None);
    };
    let Some(record) = directory.open_pinned_regular(
        std::ffi::OsStr::new(SYNC_APPLY_QUARANTINE_REPORT_FILE),
        false,
    )?
    else {
        return Ok(None);
    };
    let observation = record.observation()?;
    let raw = record.read_stable_bounded(&observation, MAX_SYNC_APPLY_QUARANTINE_REPORT_BYTES)?;
    serde_json::from_slice(&raw)
        .map(Some)
        .context("sync apply quarantine report is unreadable")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(job_id: &str) -> SyncApplyQuarantineEntry {
        SyncApplyQuarantineEntry {
            job_id: job_id.to_owned(),
            code: "journal_unreadable".to_owned(),
            detail: "schema 1".to_owned(),
            quarantined_at: "2026-11-20T00:00:00Z".to_owned(),
            project_path: Some("/p".to_owned()),
            transaction_id: Some("t".to_owned()),
            transaction_path: Some("/p/.ai/state/project-apply-transactions/t".to_owned()),
        }
    }

    #[test]
    fn report_round_trips_is_bounded_and_an_empty_set_removes_it() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_report(dir.path()).unwrap().is_none());
        let jobs = vec![entry("a"), entry("b")];
        write_report(dir.path(), jobs.clone()).unwrap();
        let report = read_report(dir.path()).unwrap().unwrap();
        assert_eq!(report.jobs, jobs);
        assert_eq!(report.omitted, 0);

        let many = (0..MAX_SYNC_APPLY_QUARANTINE_ENTRIES + 5)
            .map(|i| {
                let mut e = entry(&format!("j{i}"));
                e.detail = "x".repeat(MAX_SYNC_APPLY_QUARANTINE_DETAIL_CHARS + 10);
                e
            })
            .collect();
        write_report(dir.path(), many).unwrap();
        let report = read_report(dir.path()).unwrap().unwrap();
        assert_eq!(report.jobs.len(), MAX_SYNC_APPLY_QUARANTINE_ENTRIES);
        assert_eq!(report.omitted, 5);
        assert_eq!(
            report.jobs[0].detail.chars().count(),
            MAX_SYNC_APPLY_QUARANTINE_DETAIL_CHARS
        );

        write_report(dir.path(), Vec::new()).unwrap();
        assert!(read_report(dir.path()).unwrap().is_none());
        write_report(dir.path(), Vec::new()).unwrap();
    }

    #[test]
    fn unknown_fields_are_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            report_path(dir.path()),
            br#"{"written_at":"x","jobs":[],"omitted":0,"extra":1}"#,
        )
        .unwrap();
        read_report(dir.path()).expect_err("deny_unknown_fields");
    }
}
