//! Workload-independent restore document carried by a generic `StateManifest`.

use std::collections::BTreeMap;

use anyhow::bail;
use serde::{Deserialize, Serialize};

use super::{ExecutionProjectAuthority, PortableSessionStateContract};

pub const WORKER_SESSION_RESTORE_CONTRACT: &str = "ryeos.worker_session.restore.v1";
pub const WORKER_SESSION_RESTORE_KIND: &str = "worker_session_restore";
pub const WORKER_SESSION_RESTORE_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSessionDependencyRestore {
    pub exact_program_hash: String,
    pub source_capsule_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSessionCheckpointPosition {
    pub chain_root_id: String,
    pub placement_thread_id: String,
    pub chain_seq: u64,
    pub event_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSessionPortableStateRestore {
    pub selector_contract: PortableSessionStateContract,
    pub selector_contract_digest: String,
    pub attachment_name: String,
    pub incoming_tree_hash: String,
    pub expected_predecessor_manifest_hash: Option<String>,
    pub expected_predecessor_tree_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSessionRestore {
    pub schema: u32,
    pub kind: String,
    pub contract: String,
    pub outer_exact_program_hash: String,
    pub persistent_dependencies: BTreeMap<String, WorkerSessionDependencyRestore>,
    pub upstream_session_id: String,
    pub source_position: WorkerSessionCheckpointPosition,
    pub source_project_authority: ExecutionProjectAuthority,
    pub project_candidate_snapshot_hash: Option<String>,
    pub portable_state: WorkerSessionPortableStateRestore,
    pub pending_contact_settlement_digest: String,
    pub credential_subject_contract_digest: String,
    pub credential_subject_digest: String,
    pub source_site_id: String,
    pub source_launch_capsule_hash: String,
}

impl WorkerSessionRestore {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != WORKER_SESSION_RESTORE_SCHEMA
            || self.kind != WORKER_SESSION_RESTORE_KIND
            || self.contract != WORKER_SESSION_RESTORE_CONTRACT
        {
            bail!("worker-session restore is not the exact current contract");
        }
        for (label, value) in [
            ("upstream_session_id", self.upstream_session_id.as_str()),
            (
                "source chain_root_id",
                self.source_position.chain_root_id.as_str(),
            ),
            (
                "source placement_thread_id",
                self.source_position.placement_thread_id.as_str(),
            ),
            ("source_site_id", self.source_site_id.as_str()),
            (
                "portable-state attachment name",
                self.portable_state.attachment_name.as_str(),
            ),
        ] {
            super::validate_trimmed_control_free(&format!("worker-session {label}"), value, false)?;
            if value.len() > 4096 {
                bail!("worker-session {label} exceeds its byte ceiling");
            }
        }
        if self.source_position.chain_seq == 0 {
            bail!("worker-session restore source position must be nonzero");
        }
        for (label, hash) in [
            (
                "outer exact program",
                self.outer_exact_program_hash.as_str(),
            ),
            ("source event", self.source_position.event_hash.as_str()),
            (
                "portable-state selector contract",
                self.portable_state.selector_contract_digest.as_str(),
            ),
            (
                "portable-state incoming tree",
                self.portable_state.incoming_tree_hash.as_str(),
            ),
            (
                "pending-contact settlement",
                self.pending_contact_settlement_digest.as_str(),
            ),
            (
                "credential-subject contract",
                self.credential_subject_contract_digest.as_str(),
            ),
            (
                "credential subject",
                self.credential_subject_digest.as_str(),
            ),
            (
                "source launch capsule",
                self.source_launch_capsule_hash.as_str(),
            ),
        ] {
            super::thread_snapshot::validate_canonical_hash(
                &format!("worker-session restore {label}"),
                hash,
            )?;
        }
        for (label, hash) in [
            (
                "project candidate",
                self.project_candidate_snapshot_hash.as_deref(),
            ),
            (
                "predecessor manifest",
                self.portable_state
                    .expected_predecessor_manifest_hash
                    .as_deref(),
            ),
            (
                "predecessor tree",
                self.portable_state
                    .expected_predecessor_tree_hash
                    .as_deref(),
            ),
        ] {
            if let Some(hash) = hash {
                super::thread_snapshot::validate_canonical_hash(
                    &format!("worker-session restore {label}"),
                    hash,
                )?;
            }
        }
        if self
            .portable_state
            .expected_predecessor_manifest_hash
            .is_some()
            != self.portable_state.expected_predecessor_tree_hash.is_some()
        {
            bail!("worker-session predecessor manifest and tree must be paired");
        }
        self.source_project_authority.validate()?;
        self.portable_state.selector_contract.validate()?;
        let selector_digest = lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(
                &self.portable_state.selector_contract,
            )?)?
            .as_bytes(),
        );
        if selector_digest != self.portable_state.selector_contract_digest {
            bail!("worker-session selector contract digest mismatch");
        }
        if self.persistent_dependencies.is_empty() || self.persistent_dependencies.len() > 64 {
            bail!("worker-session restore has an invalid dependency count");
        }
        for (name, dependency) in &self.persistent_dependencies {
            super::validate_trimmed_control_free("worker-session dependency name", name, false)?;
            if name.len() > 256 {
                bail!("worker-session dependency name exceeds its byte ceiling");
            }
            super::thread_snapshot::validate_canonical_hash(
                "worker-session dependency exact program",
                &dependency.exact_program_hash,
            )?;
            super::thread_snapshot::validate_canonical_hash(
                "worker-session dependency source capsule",
                &dependency.source_capsule_hash,
            )?;
        }
        Ok(())
    }

    pub fn to_value(&self) -> anyhow::Result<serde_json::Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn from_current_value(value: serde_json::Value) -> anyhow::Result<Self> {
        let restore: Self = serde_json::from_value(value)?;
        restore.validate()?;
        Ok(restore)
    }
}
