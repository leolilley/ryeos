//! Admission evidence: finalizer decisions as durable facts.
//!
//! Every real managed-launch attempt appends one durable event to a
//! node-owned bundle-event chain — `admission_recorded` when a launch was
//! finalized and spawned, `admission_refused` when it failed before spawn,
//! with a closed stage vocabulary mirroring the effective-program failure
//! classification. Preview/projection paths never append (they are polled
//! operator browsing, not history).
//!
//! Forgery posture: the chain lives under a reserved pseudo-bundle id that no
//! signed manifest owns, and the capability lane refuses the reserved id
//! outright (`bundle_event_service::effective_bundle_id`), so only this
//! daemon-internal module can author admission history. Evidence is served
//! through the field projection, not raw chain reads.
//!
//! Failure posture: emission failure is an evidence gap, never a launch
//! failure — admission history must not couple launch availability to the
//! event store. Gaps surface as daemon warnings.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::state_store::StateStore;

/// Reserved pseudo-bundle for daemon-authored admission history. The
/// capability lane refuses this id for every bundle-event operation.
pub const ADMISSION_BUNDLE_ID: &str = "ryeos-node";
pub const ADMISSION_EVENT_KIND: &str = "admission";
pub const ADMISSION_RECORDED_EVENT_TYPE: &str = "admission_recorded";
pub const ADMISSION_REFUSED_EVENT_TYPE: &str = "admission_refused";
pub const ADMISSION_EVENT_SCHEMA_VERSION: u32 = 1;
/// Bound on any single free-text detail carried in an admission payload.
const MAX_ADMISSION_DETAIL_BYTES: usize = 2048;
/// Bound on the complete serialized admission payload.
const MAX_ADMISSION_PAYLOAD_BYTES: usize = 8 * 1024;

/// Closed refusal-stage vocabulary mirroring the effective-program failure
/// classification. A new failure class extends this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionStage {
    Materialization,
    Secrets,
    Authority,
    Preparation,
    Cancelled,
    Internal,
}

impl AdmissionStage {
    pub fn as_str(self) -> &'static str {
        match self {
            AdmissionStage::Materialization => "materialization",
            AdmissionStage::Secrets => "secrets",
            AdmissionStage::Authority => "authority",
            AdmissionStage::Preparation => "preparation",
            AdmissionStage::Cancelled => "cancelled",
            AdmissionStage::Internal => "internal",
        }
    }
}

/// Stable per-project admission chain id derived from the canonical project
/// root string. Path-shaped input never appears in the id itself.
pub fn admission_chain_id(project_path: &Path) -> String {
    let canonical = project_path
        .canonicalize()
        .unwrap_or_else(|_| project_path.to_path_buf());
    let digest = lillux::cas::sha256_hex(canonical.to_string_lossy().as_bytes());
    format!("project-{digest}")
}

fn truncate_detail(detail: &str) -> String {
    if detail.len() <= MAX_ADMISSION_DETAIL_BYTES {
        return detail.to_string();
    }
    let mut end = MAX_ADMISSION_DETAIL_BYTES;
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &detail[..end])
}

pub struct AdmissionRecorded<'a> {
    pub project_path: &'a Path,
    pub canonical_ref: &'a str,
    pub thread_id: &'a str,
    pub chain_root_id: &'a str,
    pub root_raw_content_digest: &'a str,
    pub effective_definition_digest: &'a str,
    pub admitted_launch_capsule_hash: &'a str,
    pub acting_principal: &'a str,
}

pub struct AdmissionRefused<'a> {
    pub project_path: &'a Path,
    pub canonical_ref: &'a str,
    pub stage: AdmissionStage,
    pub reason_code: &'a str,
    pub detail: &'a str,
    pub acting_principal: &'a str,
}

fn append(
    state_store: &StateStore,
    project_path: &Path,
    event_type: &str,
    payload: Value,
) -> Result<()> {
    let serialized = serde_json::to_vec(&payload)?;
    if serialized.len() > MAX_ADMISSION_PAYLOAD_BYTES {
        anyhow::bail!(
            "admission payload is {} bytes; cap is {MAX_ADMISSION_PAYLOAD_BYTES}",
            serialized.len()
        );
    }
    state_store.append_daemon_bundle_event_at_current_head(
        ryeos_state::BundleEventAppendRequest {
            effective_bundle_id: ADMISSION_BUNDLE_ID.to_string(),
            bundle_id: None,
            event_kind: ADMISSION_EVENT_KIND.to_string(),
            chain_id: admission_chain_id(project_path),
            event_type: event_type.to_string(),
            schema_version: ADMISSION_EVENT_SCHEMA_VERSION,
            payload,
            expected_chain_head_hash: None,
            idempotency_key: None,
            correlation_id: None,
            causation_id: None,
            attribution: ryeos_state::objects::BundleEventAttribution {
                actor: Some("ryeos-daemon".to_string()),
                tool: None,
                executor: Some("admission".to_string()),
                site: None,
            },
            attachments: vec![],
        },
    )?;
    Ok(())
}

/// Record a finalized-and-spawned managed launch. Emission failure is logged
/// by the caller as a warning; it never fails the launch.
pub fn append_admission_recorded(
    state_store: &StateStore,
    event: AdmissionRecorded<'_>,
) -> Result<()> {
    append(
        state_store,
        event.project_path,
        ADMISSION_RECORDED_EVENT_TYPE,
        json!({
            "canonical_ref": event.canonical_ref,
            "thread_id": event.thread_id,
            "chain_root_id": event.chain_root_id,
            "root_raw_content_digest": event.root_raw_content_digest,
            "effective_definition_digest": event.effective_definition_digest,
            "admitted_launch_capsule_hash": event.admitted_launch_capsule_hash,
            "acting_principal": event.acting_principal,
        }),
    )
}

/// Record a managed launch refused before spawn.
pub fn append_admission_refused(
    state_store: &StateStore,
    event: AdmissionRefused<'_>,
) -> Result<()> {
    append(
        state_store,
        event.project_path,
        ADMISSION_REFUSED_EVENT_TYPE,
        json!({
            "canonical_ref": event.canonical_ref,
            "stage": event.stage.as_str(),
            "reason_code": truncate_detail(event.reason_code),
            "detail": truncate_detail(event.detail),
            "acting_principal": event.acting_principal,
        }),
    )
}

/// Principal-scoped, bounded observation of one project's daemon-authored
/// admissions. Other principals' payloads never leave this owner for UI
/// projection. A signed chain cursor locates older pages; it is not an
/// assertion that missing admissions were never attempted or emitted.
pub struct PrincipalAdmissionEvents {
    pub records: Vec<ryeos_state::BundleEventRecord>,
    pub incomplete: bool,
}

fn admission_payload_owned_by(payload: &Value, principal: &str) -> bool {
    payload.get("acting_principal").and_then(Value::as_str) == Some(principal)
}

/// Principals are matched exactly as the emit sites recorded them: durable
/// callers by their canonical `fp:<fingerprint>` id, browser sessions by their
/// `session:<id>` id. The reader never normalises or guesses between forms.
pub fn read_admission_events_for_principal(
    state_store: &StateStore,
    project_path: &Path,
    principal: &str,
    limit: usize,
) -> Result<PrincipalAdmissionEvents> {
    anyhow::ensure!(!principal.is_empty(), "admission principal must be present");
    anyhow::ensure!((1..=50).contains(&limit), "admission limit is invalid");
    const MAX_SCANNED: usize = 500;
    const PAGE_SIZE: usize = 50;
    let chain_id = admission_chain_id(project_path);
    let mut cursor = None;
    let mut scanned = 0usize;
    let mut records = Vec::new();
    let mut extra_owned = false;
    while scanned < MAX_SCANNED && records.len() < limit {
        let page_limit = PAGE_SIZE.min(MAX_SCANNED - scanned);
        let page = state_store.read_bundle_event_chain_page(
            ADMISSION_BUNDLE_ID,
            ADMISSION_EVENT_KIND,
            &chain_id,
            cursor.as_ref(),
            page_limit,
            MAX_ADMISSION_PAYLOAD_BYTES * page_limit,
        )?;
        if page.records.is_empty() && page.next_cursor.is_some() {
            anyhow::bail!("admission history page made no progress");
        }
        scanned += page.records.len();
        for record in page.records {
            if !admission_payload_owned_by(&record.event.payload, principal) {
                continue;
            }
            if records.len() < limit {
                records.push(record);
            } else {
                extra_owned = true;
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() || extra_owned {
            break;
        }
    }
    Ok(PrincipalAdmissionEvents {
        records,
        incomplete: cursor.is_some() || extra_owned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_reader_accepts_only_exact_payload_principal() {
        assert!(admission_payload_owned_by(
            &json!({"acting_principal":"owner"}),
            "owner"
        ));
        assert!(!admission_payload_owned_by(
            &json!({"acting_principal":"other","detail":"private refusal"}),
            "owner"
        ));
        assert!(!admission_payload_owned_by(&json!({}), "owner"));
        assert!(!admission_payload_owned_by(
            &json!({"acting_principal":"owner-extra"}),
            "owner"
        ));
    }

    #[test]
    fn principal_reader_returns_only_owned_admissions_and_refuses_bad_bounds() {
        let root = tempfile::tempdir().unwrap();
        let (_store, state) = crate::state::test_support::build(root.path()).unwrap();
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let fp = format!("fp:{}", "ab".repeat(32));
        let bare = "ab".repeat(32);
        let session = "session:browser-1";
        for (principal, thread) in [
            (fp.as_str(), "T-4"),
            (bare.as_str(), "T-5"),
            (session, "T-6"),
        ] {
            append_admission_recorded(
                &state.state_store,
                AdmissionRecorded {
                    project_path: &project,
                    canonical_ref: "tool:demo",
                    thread_id: thread,
                    chain_root_id: thread,
                    root_raw_content_digest: "d1",
                    effective_definition_digest: "d2",
                    admitted_launch_capsule_hash: "d3",
                    acting_principal: principal,
                },
            )
            .unwrap();
        }
        for (principal, thread) in [("owner", "T-1"), ("other", "T-2"), ("owner", "T-3")] {
            append_admission_recorded(
                &state.state_store,
                AdmissionRecorded {
                    project_path: &project,
                    canonical_ref: "tool:demo",
                    thread_id: thread,
                    chain_root_id: thread,
                    root_raw_content_digest: "d1",
                    effective_definition_digest: "d2",
                    admitted_launch_capsule_hash: "d3",
                    acting_principal: principal,
                },
            )
            .unwrap();
        }
        let owned =
            read_admission_events_for_principal(&state.state_store, &project, "owner", 10).unwrap();
        assert_eq!(owned.records.len(), 2);
        assert!(!owned.incomplete);
        assert!(
            owned
                .records
                .iter()
                .all(|record| { record.event.payload["acting_principal"] == "owner" })
        );
        for (principal, thread) in [(fp.as_str(), "T-4"), (session, "T-6")] {
            let read =
                read_admission_events_for_principal(&state.state_store, &project, principal, 10)
                    .unwrap();
            assert_eq!(read.records.len(), 1);
            assert_eq!(read.records[0].event.payload["thread_id"], thread);
        }
        // The bare fingerprint is a different principal string, not an alias.
        let bare_read =
            read_admission_events_for_principal(&state.state_store, &project, &bare, 10).unwrap();
        assert_eq!(bare_read.records.len(), 1);
        assert_eq!(bare_read.records[0].event.payload["thread_id"], "T-5");
        let capped =
            read_admission_events_for_principal(&state.state_store, &project, "owner", 1).unwrap();
        assert_eq!(capped.records.len(), 1);
        assert!(capped.incomplete);
        for (principal, limit) in [("", 1), ("owner", 0), ("owner", 51)] {
            assert!(
                read_admission_events_for_principal(&state.state_store, &project, principal, limit)
                    .is_err()
            );
        }
    }

    #[test]
    fn chain_id_is_stable_and_path_free() {
        let a = admission_chain_id(Path::new("/definitely/not/a/real/path"));
        let b = admission_chain_id(Path::new("/definitely/not/a/real/path"));
        assert_eq!(a, b);
        assert!(a.starts_with("project-"));
        assert!(!a.contains('/'));
    }

    #[test]
    fn detail_truncation_is_bounded_and_char_safe() {
        let long = "é".repeat(4096);
        let truncated = truncate_detail(&long);
        assert!(truncated.len() <= MAX_ADMISSION_DETAIL_BYTES + 16);
        assert!(truncated.ends_with("[truncated]"));
    }

    #[test]
    fn stage_vocabulary_is_closed_and_stable() {
        for stage in [
            AdmissionStage::Materialization,
            AdmissionStage::Secrets,
            AdmissionStage::Authority,
            AdmissionStage::Preparation,
            AdmissionStage::Cancelled,
            AdmissionStage::Internal,
        ] {
            assert!(!stage.as_str().is_empty());
        }
    }
}
