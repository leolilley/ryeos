//! Operator report of hosted candidates stuck in `publish_ready` whose
//! admitted base HEAD has moved.
//!
//! The dedicated-session rows and the signed project HEAD are the authority.
//! `ryeos node doctor` runs offline and never opens the runtime database, so
//! the daemon mirrors the stale sessions into this report at startup and after
//! each periodic recovery pass, and removes it when none remain. Nothing here
//! settles a session: each one settles to `base_superseded` on its owner's
//! next publish, through that publish's own owner-authorized root.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const STALE_PUBLICATION_REPORT_FILE: &str = "stale-publication.json";
const MAX_STALE_PUBLICATION_REPORT_BYTES: u64 = 1024 * 1024;

/// One `publish_ready` session whose admitted base is no longer HEAD.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StalePublishReadySession {
    pub chain_root_id: String,
    pub placement_thread_id: String,
    pub candidate_snapshot_hash: String,
    pub base_snapshot_hash: String,
    pub current_head: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StalePublicationReport {
    pub written_at: String,
    pub sessions: Vec<StalePublishReadySession>,
}

/// The same test a live publish makes before its CAS: HEAD is neither the
/// admitted base nor the candidate, so the candidate can never publish.
pub fn base_has_moved(current_head: Option<&str>, base: &str, candidate: &str) -> bool {
    current_head != Some(base) && current_head != Some(candidate)
}

pub fn report_path(state_dir: &Path) -> PathBuf {
    state_dir.join(STALE_PUBLICATION_REPORT_FILE)
}

/// Mirror the current stale sessions. An empty set removes the report.
pub fn write_report(state_dir: &Path, sessions: Vec<StalePublishReadySession>) -> Result<()> {
    let path = report_path(state_dir);
    if sessions.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("remove stale publication report {}", path.display())),
        };
    }
    let report = StalePublicationReport {
        written_at: lillux::time::iso8601_now(),
        sessions,
    };
    let bytes = serde_json::to_vec_pretty(&report)?;
    anyhow::ensure!(
        u64::try_from(bytes.len())? <= MAX_STALE_PUBLICATION_REPORT_BYTES,
        "stale publication report exceeds its bound"
    );
    lillux::atomic_fs::atomic_write(&path, &bytes)
        .with_context(|| format!("write stale publication report {}", path.display()))
}

/// Read the last mirrored report. `None` when the daemon recorded none.
pub fn read_report(state_dir: &Path) -> Result<Option<StalePublicationReport>> {
    let Some(directory) = lillux::PinnedDirectory::open(state_dir)? else {
        return Ok(None);
    };
    let Some(record) = directory
        .open_pinned_regular(std::ffi::OsStr::new(STALE_PUBLICATION_REPORT_FILE), false)?
    else {
        return Ok(None);
    };
    let observation = record.observation()?;
    let raw = record.read_stable_bounded(&observation, MAX_STALE_PUBLICATION_REPORT_BYTES)?;
    serde_json::from_slice(&raw)
        .map(Some)
        .context("stale publication report is unreadable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_head_off_both_base_and_candidate_is_stale() {
        let (base, candidate, other) = ("b".repeat(64), "c".repeat(64), "e".repeat(64));
        assert!(!base_has_moved(Some(&base), &base, &candidate));
        assert!(!base_has_moved(Some(&candidate), &base, &candidate));
        assert!(base_has_moved(Some(&other), &base, &candidate));
        assert!(base_has_moved(None, &base, &candidate));
    }

    #[test]
    fn report_round_trips_and_an_empty_set_removes_it() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_report(dir.path()).unwrap().is_none());
        let sessions = vec![StalePublishReadySession {
            chain_root_id: "T-root".into(),
            placement_thread_id: "T-placement".into(),
            candidate_snapshot_hash: "c".repeat(64),
            base_snapshot_hash: "b".repeat(64),
            current_head: Some("e".repeat(64)),
        }];
        write_report(dir.path(), sessions.clone()).unwrap();
        assert_eq!(read_report(dir.path()).unwrap().unwrap().sessions, sessions);
        write_report(dir.path(), Vec::new()).unwrap();
        assert!(read_report(dir.path()).unwrap().is_none());
        write_report(dir.path(), Vec::new()).unwrap();
    }
}
