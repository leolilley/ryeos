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

fn append(state_store: &StateStore, project_path: &Path, event_type: &str, payload: Value) -> Result<()> {
    let serialized = serde_json::to_vec(&payload)?;
    if serialized.len() > MAX_ADMISSION_PAYLOAD_BYTES {
        anyhow::bail!(
            "admission payload is {} bytes; cap is {MAX_ADMISSION_PAYLOAD_BYTES}",
            serialized.len()
        );
    }
    state_store.append_bundle_event_with_attachments(
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
        vec![],
    )?;
    Ok(())
}

/// Record a finalized-and-spawned managed launch. Emission failure is logged
/// by the caller as a warning; it never fails the launch.
pub fn append_admission_recorded(state_store: &StateStore, event: AdmissionRecorded<'_>) -> Result<()> {
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
pub fn append_admission_refused(state_store: &StateStore, event: AdmissionRefused<'_>) -> Result<()> {
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

/// Read the newest admission events for a project (for the field projection).
pub fn read_admission_events(
    state_store: &StateStore,
    project_path: &Path,
    limit: usize,
) -> Result<ryeos_state::BundleEventChainPage> {
    state_store.read_bundle_event_chain_page(
        ADMISSION_BUNDLE_ID,
        ADMISSION_EVENT_KIND,
        &admission_chain_id(project_path),
        None,
        limit,
        MAX_ADMISSION_PAYLOAD_BYTES * limit.max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
