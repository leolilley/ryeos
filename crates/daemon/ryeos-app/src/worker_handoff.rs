//! Generic cross-site worker-placement authority.
//!
//! The wire object remains RyeOS's ordinary signed `Attestation`. These
//! structures make the evidence under the two handoff policies exhaustive and
//! typed so mutable sync-job phases or mere trust-store membership can never
//! be interpreted as placement or chain-writer authority.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use ryeos_state::objects::{AdmittedAccountingScope, Attestation, ExecutionProjectAuthority};
use ryeos_state::signer::Signer;

use crate::launch_metadata::{OriginalPushedHeadRef, ResumeContext, StableProjectIdentity};

pub const WORKER_PLACEMENT_POLICY: &str = "worker-placement-v1";
pub const WORKER_PLACEMENT_CLAIM: &str = "admitted";
pub const WORKER_SESSION_HANDOFF_OPERATION: &str = "worker_session_handoff";

const PLACEMENT_EVIDENCE_SCHEMA: &str = "ryeos.worker_placement_admission.v1";
const HANDOFF_JOB_SCHEMA: &str = "ryeos.worker_session_handoff_job.v1";
const HANDOFF_PROGRESS_SCHEMA: &str = "ryeos.worker_session_handoff_progress.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerHandoffJobRole {
    Source,
    Target,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerHandoffPhase {
    Planned,
    SourceExported,
    TargetPrepared,
    SourceCommitted,
    TargetAdopted,
    StateInstalled,
    ProcessAttached,
    Completed,
}

impl WorkerHandoffPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::SourceExported => "source_exported",
            Self::TargetPrepared => "target_prepared",
            Self::SourceCommitted => "source_committed",
            Self::TargetAdopted => "target_adopted",
            Self::StateInstalled => "state_installed",
            Self::ProcessAttached => "process_attached",
            Self::Completed => "completed",
        }
    }
}

/// Immutable, bounded operation body retained in the existing sync-job row on
/// each participating node. Large launch bytes live in
/// `PlacementTransferManifest`; mutable testimony lives in
/// `WorkerSessionHandoffProgress` and never replaces chain authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSessionHandoffJobOperation {
    pub schema: String,
    pub operation_type: String,
    pub role: WorkerHandoffJobRole,
    pub operation_id: String,
    pub owner_principal: String,
    pub chain_root_id: String,
    pub origin_site_id: String,
    pub source_site_id: String,
    pub target_site_id: String,
    pub source_placement_thread_id: String,
    pub successor_placement_thread_id: String,
    pub source_chain_head_hash: String,
    pub source_last_event_hash: String,
    pub checkpoint_manifest_hash: String,
    pub transfer_manifest_hash: String,
    pub peer_remote_name: String,
    pub source_project_path: String,
    pub target_project_path: String,
    pub project_route_digest: String,
    pub target_credential_profile_id: String,
}

impl WorkerSessionHandoffJobOperation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        role: WorkerHandoffJobRole,
        operation_id: String,
        owner_principal: String,
        chain_root_id: String,
        origin_site_id: String,
        source_site_id: String,
        target_site_id: String,
        source_placement_thread_id: String,
        successor_placement_thread_id: String,
        source_chain_head_hash: String,
        source_last_event_hash: String,
        checkpoint_manifest_hash: String,
        transfer_manifest_hash: String,
        peer_remote_name: String,
        source_project_path: String,
        target_project_path: String,
        project_route_digest: String,
        target_credential_profile_id: String,
    ) -> anyhow::Result<Self> {
        let operation = Self {
            schema: HANDOFF_JOB_SCHEMA.to_owned(),
            operation_type: WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
            role,
            operation_id,
            owner_principal,
            chain_root_id,
            origin_site_id,
            source_site_id,
            target_site_id,
            source_placement_thread_id,
            successor_placement_thread_id,
            source_chain_head_hash,
            source_last_event_hash,
            checkpoint_manifest_hash,
            transfer_manifest_hash,
            peer_remote_name,
            source_project_path,
            target_project_path,
            project_route_digest,
            target_credential_profile_id,
        };
        operation.validate()?;
        Ok(operation)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != HANDOFF_JOB_SCHEMA
            || self.operation_type != WORKER_SESSION_HANDOFF_OPERATION
        {
            bail!("worker handoff job is not the exact current contract");
        }
        validate_common(
            &self.operation_id,
            &self.owner_principal,
            &self.chain_root_id,
            &self.origin_site_id,
            &self.source_site_id,
            &self.target_site_id,
            &self.source_placement_thread_id,
            &self.successor_placement_thread_id,
            &self.source_chain_head_hash,
            &self.source_last_event_hash,
            &self.checkpoint_manifest_hash,
            &self.transfer_manifest_hash,
        )?;
        hash("handoff project route", &self.project_route_digest)?;
        for (label, value) in [
            ("peer remote", self.peer_remote_name.as_str()),
            ("source project path", self.source_project_path.as_str()),
            ("target project path", self.target_project_path.as_str()),
            (
                "target credential profile",
                self.target_credential_profile_id.as_str(),
            ),
        ] {
            label_value(label, value)?;
        }
        for (label, path) in [
            ("source project path", self.source_project_path.as_str()),
            ("target project path", self.target_project_path.as_str()),
        ] {
            if !std::path::Path::new(path).is_absolute() {
                bail!("worker handoff {label} is not absolute");
            }
        }
        Ok(())
    }

    pub fn to_value(&self) -> anyhow::Result<serde_json::Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn from_value(value: serde_json::Value) -> anyhow::Result<Self> {
        let operation: Self = serde_json::from_value(value)?;
        operation.validate()?;
        Ok(operation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSessionHandoffProgress {
    pub schema: String,
    pub operation_id: String,
    pub phase: WorkerHandoffPhase,
    pub placement_attestation_hash: Option<String>,
    pub target_runtime_seed_hash: Option<String>,
    pub writer_grant_hash: Option<String>,
    pub target_chain_head_hash: Option<String>,
    pub credential_reservation_id: Option<String>,
}

impl WorkerSessionHandoffProgress {
    pub fn planned(operation_id: String) -> anyhow::Result<Self> {
        let progress = Self {
            schema: HANDOFF_PROGRESS_SCHEMA.to_owned(),
            operation_id,
            phase: WorkerHandoffPhase::Planned,
            placement_attestation_hash: None,
            target_runtime_seed_hash: None,
            writer_grant_hash: None,
            target_chain_head_hash: None,
            credential_reservation_id: None,
        };
        progress.validate()?;
        Ok(progress)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != HANDOFF_PROGRESS_SCHEMA {
            bail!("worker handoff progress is not the exact current contract");
        }
        hash("handoff progress operation", &self.operation_id)?;
        for (label, value) in [
            (
                "placement attestation",
                self.placement_attestation_hash.as_deref(),
            ),
            (
                "target runtime seed",
                self.target_runtime_seed_hash.as_deref(),
            ),
            ("writer grant", self.writer_grant_hash.as_deref()),
            ("target chain head", self.target_chain_head_hash.as_deref()),
        ] {
            if let Some(value) = value {
                hash(label, value)?;
            }
        }
        if let Some(reservation) = &self.credential_reservation_id {
            label_value("credential reservation", reservation)?;
        }
        if self.phase >= WorkerHandoffPhase::TargetPrepared
            && (self.placement_attestation_hash.is_none()
                || self.target_runtime_seed_hash.is_none()
                || self.credential_reservation_id.is_none())
        {
            bail!("target-prepared handoff progress is incomplete");
        }
        if self.phase >= WorkerHandoffPhase::SourceCommitted
            && (self.writer_grant_hash.is_none() || self.target_chain_head_hash.is_none())
        {
            bail!("source-committed handoff progress is incomplete");
        }
        Ok(())
    }

    pub fn to_value(&self) -> anyhow::Result<serde_json::Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn from_value(value: serde_json::Value) -> anyhow::Result<Self> {
        let progress: Self = serde_json::from_value(value)?;
        progress.validate()?;
        Ok(progress)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialGenerationReservation {
    pub profile_id: String,
    pub owner_principal: String,
    pub generation: u64,
    pub reservation_id: String,
    /// Opaque workload-session coordinate that the target worker must recover.
    /// It is provider-neutral and never interpreted by RyeOS.
    pub upstream_session_id: String,
    pub subject_contract_digest: String,
    pub subject_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectAuthorityRebind {
    pub route_digest: String,
    pub source_stable_project_identity: String,
    pub target_stable_project_identity: String,
    pub source_candidate_snapshot_hash: String,
    pub source_base_snapshot_hash: String,
    pub target_expected_head_hash: Option<String>,
    pub target_authority: ExecutionProjectAuthority,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountingConservation {
    pub source_scope: Option<AdmittedAccountingScope>,
    pub target_scope: Option<AdmittedAccountingScope>,
    pub source_financial_high_water: u64,
    pub source_charged_usd_nanos: u64,
    pub source_remaining_cap_usd_nanos: Option<u64>,
    pub target_cap_usd_nanos: Option<u64>,
    pub source_remaining_directive_cap_usd_nanos: Option<u64>,
    pub target_directive_cap_usd_nanos: Option<u64>,
}

/// Node-local operands needed to derive a target `ResumeContext` from an
/// already admitted source. This is deliberately not a second durable
/// authority object: the resulting complete resume ledger is sealed into the
/// target launch capsule, while `WorkerPlacementAdmissionEvidence` binds its
/// project, credential, site, and accounting substitutions.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteResumeContextRebind {
    pub source_site_id: String,
    pub target_site_id: String,
    pub target_project_context: ryeos_engine::contracts::ProjectContext,
    pub target_project_authority: ExecutionProjectAuthority,
    pub target_stable_project_identity: Option<StableProjectIdentity>,
    pub target_local_overlay_root: Option<PathBuf>,
    pub target_original_snapshot_hash: Option<String>,
    pub target_original_pushed_head_ref: Option<OriginalPushedHeadRef>,
    pub target_state_root: Option<PathBuf>,
    pub source_credential_profile_id: String,
    pub credential_reservation: CredentialGenerationReservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementAdmissionEvidence {
    pub schema: String,
    pub operation_type: String,
    pub operation_id: String,
    pub owner_principal: String,
    pub chain_root_id: String,
    pub origin_site_id: String,
    pub source_site_id: String,
    pub target_site_id: String,
    pub source_placement_thread_id: String,
    pub successor_placement_thread_id: String,
    pub source_chain_head_hash: String,
    pub source_last_event_hash: String,
    pub checkpoint_manifest_hash: String,
    pub outer_exact_program_hash: String,
    pub persistent_dependency_programs: BTreeMap<String, String>,
    pub target_persistent_session_capsules: BTreeMap<String, String>,
    pub target_execution_realization_hash: String,
    pub credential_reservation: CredentialGenerationReservation,
    pub project_rebind: ProjectAuthorityRebind,
    pub accounting: AccountingConservation,
    pub target_launch_capsule_hash: String,
    /// Immutable CAS recovery seed for the exact successor placement. The
    /// seed is independently reachable from the continuation edge; this hash
    /// binds target admission to those exact runtime bytes before source cut.
    pub target_runtime_seed_hash: String,
}

impl WorkerPlacementAdmissionEvidence {
    pub fn new(
        operation_id: String,
        owner_principal: String,
        chain_root_id: String,
        origin_site_id: String,
        source_site_id: String,
        target_site_id: String,
        source_placement_thread_id: String,
        successor_placement_thread_id: String,
        source_chain_head_hash: String,
        source_last_event_hash: String,
        checkpoint_manifest_hash: String,
        outer_exact_program_hash: String,
        persistent_dependency_programs: BTreeMap<String, String>,
        target_persistent_session_capsules: BTreeMap<String, String>,
        target_execution_realization_hash: String,
        credential_reservation: CredentialGenerationReservation,
        project_rebind: ProjectAuthorityRebind,
        accounting: AccountingConservation,
        target_launch_capsule_hash: String,
        target_runtime_seed_hash: String,
    ) -> Self {
        Self {
            schema: PLACEMENT_EVIDENCE_SCHEMA.to_owned(),
            operation_type: WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
            operation_id,
            owner_principal,
            chain_root_id,
            origin_site_id,
            source_site_id,
            target_site_id,
            source_placement_thread_id,
            successor_placement_thread_id,
            source_chain_head_hash,
            source_last_event_hash,
            checkpoint_manifest_hash,
            outer_exact_program_hash,
            persistent_dependency_programs,
            target_persistent_session_capsules,
            target_execution_realization_hash,
            credential_reservation,
            project_rebind,
            accounting,
            target_launch_capsule_hash,
            target_runtime_seed_hash,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PLACEMENT_EVIDENCE_SCHEMA
            || self.operation_type != WORKER_SESSION_HANDOFF_OPERATION
        {
            bail!("worker placement evidence is not the exact current contract");
        }
        validate_common(
            &self.operation_id,
            &self.owner_principal,
            &self.chain_root_id,
            &self.origin_site_id,
            &self.source_site_id,
            &self.target_site_id,
            &self.source_placement_thread_id,
            &self.successor_placement_thread_id,
            &self.source_chain_head_hash,
            &self.source_last_event_hash,
            &self.checkpoint_manifest_hash,
            &self.target_launch_capsule_hash,
        )?;
        hash("outer exact program", &self.outer_exact_program_hash)?;
        hash(
            "target execution realization",
            &self.target_execution_realization_hash,
        )?;
        hash("target runtime seed", &self.target_runtime_seed_hash)?;
        if self.source_site_id == self.target_site_id {
            bail!("cross-site worker placement must change current site");
        }
        validate_dependency_maps(
            &self.persistent_dependency_programs,
            &self.target_persistent_session_capsules,
        )?;
        self.credential_reservation.validate()?;
        self.project_rebind.validate()?;
        self.accounting.validate()?;
        if let (Some(source), Some(target)) =
            (&self.accounting.source_scope, &self.accounting.target_scope)
            && (source.budget_authority_site_id != self.source_site_id
                || target.budget_authority_site_id != self.target_site_id)
        {
            bail!("accounting scopes do not belong to their admitted source and target sites");
        }
        Ok(())
    }

    pub fn sign_attestation(&self, signer: &dyn Signer) -> anyhow::Result<Attestation> {
        self.validate()?;
        Attestation::unsigned(
            self.target_launch_capsule_hash.clone(),
            WORKER_PLACEMENT_CLAIM.to_owned(),
            WORKER_PLACEMENT_POLICY.to_owned(),
            lillux::time::iso8601_now(),
            None,
            serde_json::to_value(self).context("serialize worker placement evidence")?,
        )
        .sign(signer)
    }

    pub fn from_attestation(attestation: &Attestation) -> anyhow::Result<Self> {
        if attestation.policy != WORKER_PLACEMENT_POLICY
            || attestation.claim != WORKER_PLACEMENT_CLAIM
        {
            bail!("attestation is not a worker placement admission");
        }
        let evidence: Self = serde_json::from_value(attestation.evidence.clone())
            .context("decode worker placement evidence")?;
        evidence.validate()?;
        if attestation.subject_hash != evidence.target_launch_capsule_hash {
            bail!("worker placement subject is not its target launch capsule");
        }
        Ok(evidence)
    }
}

/// Canonical runtime seed bytes prepared for publication in the private CAS.
/// The object carries only bounded coordinates and a blob edge, keeping
/// generic object traversal bounded even when the retained exact program is
/// larger than the ordinary object ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPlacementRuntimeSeed {
    pub object: ryeos_state::objects::PlacementRuntimeSeed,
    pub launch_metadata_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPlacementTransferManifest {
    pub object: ryeos_state::objects::PlacementTransferManifest,
    pub source_launch_metadata_bytes: Vec<u8>,
}

impl PreparedPlacementTransferManifest {
    pub fn object_hash(&self) -> anyhow::Result<String> {
        self.object.content_hash()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_placement_transfer_manifest(
    operation_id: &str,
    owner_principal: &str,
    chain_root_id: &str,
    origin_site_id: &str,
    source_site_id: &str,
    target_site_id: &str,
    source_placement_thread_id: &str,
    successor_placement_thread_id: &str,
    source_chain_head_hash: &str,
    source_last_event_hash: &str,
    checkpoint_manifest_hash: &str,
    source_launch_capsule_hash: &str,
    source_launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
) -> anyhow::Result<PreparedPlacementTransferManifest> {
    source_launch_metadata.validate()?;
    let value = serde_json::to_value(source_launch_metadata)
        .context("serialize source placement launch metadata")?;
    let canonical = lillux::canonical_json(&value)
        .context("canonicalize source placement launch metadata")?
        .into_bytes();
    let size = u64::try_from(canonical.len()).context("source launch metadata size overflow")?;
    if size == 0 || size > ryeos_state::objects::MAX_PLACEMENT_RUNTIME_METADATA_BYTES {
        bail!(
            "source placement launch metadata is {size} bytes; maximum is {}",
            ryeos_state::objects::MAX_PLACEMENT_RUNTIME_METADATA_BYTES
        );
    }
    let object = ryeos_state::objects::PlacementTransferManifest::new(
        operation_id.to_owned(),
        owner_principal.to_owned(),
        chain_root_id.to_owned(),
        origin_site_id.to_owned(),
        source_site_id.to_owned(),
        target_site_id.to_owned(),
        source_placement_thread_id.to_owned(),
        successor_placement_thread_id.to_owned(),
        source_chain_head_hash.to_owned(),
        source_last_event_hash.to_owned(),
        checkpoint_manifest_hash.to_owned(),
        source_launch_capsule_hash.to_owned(),
        lillux::sha256_hex(&canonical),
        size,
    )?;
    Ok(PreparedPlacementTransferManifest {
        object,
        source_launch_metadata_bytes: canonical,
    })
}

impl PreparedPlacementRuntimeSeed {
    pub fn object_hash(&self) -> anyhow::Result<String> {
        self.object.content_hash()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_placement_runtime_seed(
    operation_id: &str,
    chain_root_id: &str,
    source_placement_thread_id: &str,
    successor_placement_thread_id: &str,
    target_site_id: &str,
    owner_principal: &str,
    target_launch_capsule_hash: &str,
    launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
) -> anyhow::Result<PreparedPlacementRuntimeSeed> {
    launch_metadata.validate()?;
    let value =
        serde_json::to_value(launch_metadata).context("serialize placement runtime seed")?;
    let canonical = lillux::canonical_json(&value)
        .context("canonicalize placement runtime seed")?
        .into_bytes();
    let size = u64::try_from(canonical.len()).context("placement runtime seed size overflow")?;
    if size == 0 || size > ryeos_state::objects::MAX_PLACEMENT_RUNTIME_METADATA_BYTES {
        bail!(
            "placement runtime metadata is {size} bytes; maximum is {}",
            ryeos_state::objects::MAX_PLACEMENT_RUNTIME_METADATA_BYTES
        );
    }
    let object = ryeos_state::objects::PlacementRuntimeSeed::new(
        operation_id.to_owned(),
        chain_root_id.to_owned(),
        source_placement_thread_id.to_owned(),
        successor_placement_thread_id.to_owned(),
        target_site_id.to_owned(),
        owner_principal.to_owned(),
        target_launch_capsule_hash.to_owned(),
        lillux::sha256_hex(&canonical),
        size,
    )?;
    Ok(PreparedPlacementRuntimeSeed {
        object,
        launch_metadata_bytes: canonical,
    })
}

impl ResumeContext {
    /// Derive the complete target-node resume ledger for one cross-site worker
    /// adoption. Every field starts as an exact source copy and only the
    /// substitutions represented by `RemoteResumeContextRebind` are applied.
    /// The ordinary continuation validator remains unchanged and therefore
    /// continues to reject every site or project-endpoint change.
    pub fn for_remote_worker_adoption(
        &self,
        rebind: &RemoteResumeContextRebind,
    ) -> anyhow::Result<Self> {
        rebind.validate_against_source(self)?;
        let mut target = self.clone();
        target.parameters = rebind_credential_profile_parameter(
            &self.parameters,
            &rebind.source_credential_profile_id,
            &rebind.credential_reservation.profile_id,
        )?;
        target.project_context = rebind.target_project_context.clone();
        target.project_authority = rebind.target_project_authority.clone();
        target.stable_project_identity = rebind.target_stable_project_identity.clone();
        target.local_overlay_root = rebind.target_local_overlay_root.clone();
        target.original_snapshot_hash = rebind.target_original_snapshot_hash.clone();
        target.original_pushed_head_ref = rebind.target_original_pushed_head_ref.clone();
        target.state_root = rebind.target_state_root.clone();
        target.current_site_id = rebind.target_site_id.clone();
        target.validate_remote_worker_adoption_from(self, rebind)?;
        Ok(target)
    }

    pub fn validate_remote_worker_adoption_from(
        &self,
        source: &Self,
        rebind: &RemoteResumeContextRebind,
    ) -> anyhow::Result<()> {
        rebind.validate_against_source(source)?;
        let mut expected = source.clone();
        expected.parameters = rebind_credential_profile_parameter(
            &source.parameters,
            &rebind.source_credential_profile_id,
            &rebind.credential_reservation.profile_id,
        )?;
        expected.project_context = rebind.target_project_context.clone();
        expected.project_authority = rebind.target_project_authority.clone();
        expected.stable_project_identity = rebind.target_stable_project_identity.clone();
        expected.local_overlay_root = rebind.target_local_overlay_root.clone();
        expected.original_snapshot_hash = rebind.target_original_snapshot_hash.clone();
        expected.original_pushed_head_ref = rebind.target_original_pushed_head_ref.clone();
        expected.state_root = rebind.target_state_root.clone();
        expected.current_site_id = rebind.target_site_id.clone();
        if self != &expected {
            bail!(
                "remote worker resume authority changed outside its typed site, project, and credential rebind"
            );
        }
        if self.principal_identifier() != rebind.credential_reservation.owner_principal {
            bail!("target credential reservation is not owned by the session principal");
        }
        self.authoritative_project_identity()?;
        Ok(())
    }
}

impl RemoteResumeContextRebind {
    fn validate_against_source(&self, source: &ResumeContext) -> anyhow::Result<()> {
        for (label, site) in [
            ("source site", self.source_site_id.as_str()),
            ("target site", self.target_site_id.as_str()),
        ] {
            label_value(label, site)?;
        }
        if self.source_site_id == self.target_site_id
            || source.current_site_id != self.source_site_id
            || source.origin_site_id.is_empty()
        {
            bail!("remote resume rebind does not describe the exact source-to-target site change");
        }
        label_value(
            "source credential profile",
            &self.source_credential_profile_id,
        )?;
        self.credential_reservation.validate()?;
        if source.principal_identifier() != self.credential_reservation.owner_principal {
            bail!("credential reservation owner differs from the source session owner");
        }
        self.target_project_authority.validate()?;
        if let Some(identity) = &self.target_stable_project_identity {
            identity.validate()?;
        }
        Ok(())
    }
}

fn rebind_credential_profile_parameter(
    source: &serde_json::Value,
    expected_source_profile: &str,
    target_profile: &str,
) -> anyhow::Result<serde_json::Value> {
    label_value("source credential profile", expected_source_profile)?;
    label_value("target credential profile", target_profile)?;
    let mut target = source.clone();
    let object = target
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("worker execution parameters must be an object"))?;
    match object
        .get("credential_profile_id")
        .and_then(serde_json::Value::as_str)
    {
        Some(profile) if profile == expected_source_profile => {}
        Some(_) => bail!("source credential profile does not match the sealed worker input"),
        None => bail!("worker execution parameters have no credential_profile_id"),
    }
    object.insert(
        "credential_profile_id".to_owned(),
        serde_json::Value::String(target_profile.to_owned()),
    );
    Ok(target)
}

pub fn chain_writer_transition_from_placement(
    placement: &WorkerPlacementAdmissionEvidence,
    placement_attestation_hash: String,
    source_node_signer_fingerprint: String,
    target_node_signer_fingerprint: String,
) -> ryeos_state::objects::ChainWriterTransitionEvidence {
    ryeos_state::objects::ChainWriterTransitionEvidence {
        schema: ryeos_state::objects::CHAIN_WRITER_TRANSITION_SCHEMA,
        operation_id: placement.operation_id.clone(),
        owner_principal: placement.owner_principal.clone(),
        chain_root_id: placement.chain_root_id.clone(),
        origin_site_id: placement.origin_site_id.clone(),
        source_site_id: placement.source_site_id.clone(),
        target_site_id: placement.target_site_id.clone(),
        source_chain_head_hash: placement.source_chain_head_hash.clone(),
        source_node_signer_fingerprint,
        source_placement_thread_id: placement.source_placement_thread_id.clone(),
        source_last_event_hash: placement.source_last_event_hash.clone(),
        successor_placement_thread_id: placement.successor_placement_thread_id.clone(),
        placement_attestation_hash,
        transition_subject_hash: placement.target_launch_capsule_hash.clone(),
        target_node_signer_fingerprint,
    }
}

/// Verify the complete source-to-target capsule transition before a target
/// placement attestation can be issued. The outer program and every field of
/// the execution closure remain exact except for named, independently
/// re-admitted persistent-session capsules. Node-local project, credential,
/// realization, and accounting changes must agree with the typed placement
/// evidence and the exact target resume ledger.
#[allow(clippy::too_many_arguments)]
pub fn validate_cross_site_capsule_transition(
    source_capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
    source_resume: &ResumeContext,
    target_capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
    target_resume: &ResumeContext,
    resume_rebind: &RemoteResumeContextRebind,
    placement: &WorkerPlacementAdmissionEvidence,
    cas: &lillux::CasStore,
) -> anyhow::Result<()> {
    source_capsule.validate()?;
    target_capsule.validate()?;
    placement.validate()?;
    target_resume.validate_remote_worker_adoption_from(source_resume, resume_rebind)?;
    if !source_capsule.same_cross_site_continuation_program_admission(target_capsule)? {
        bail!("target capsule changed immutable portable worker admission");
    }
    if source_capsule.exact_program_hash != placement.outer_exact_program_hash
        || target_capsule.exact_program_hash != placement.outer_exact_program_hash
        || target_capsule.execution_realization_hash != placement.target_execution_realization_hash
        || target_capsule.content_hash()? != placement.target_launch_capsule_hash
        || source_capsule.project_authority != source_resume.project_authority
        || target_capsule.project_authority != target_resume.project_authority
        || target_capsule.project_authority != placement.project_rebind.target_authority
        || target_capsule.accounting_scope != placement.accounting.target_scope
        || source_capsule.accounting_scope != placement.accounting.source_scope
    {
        bail!("placement evidence contradicts its source or target launch capsule");
    }
    if placement.owner_principal != source_resume.principal_identifier()
        || placement.owner_principal != target_resume.principal_identifier()
        || placement.origin_site_id != source_resume.origin_site_id
        || placement.origin_site_id != target_resume.origin_site_id
        || placement.source_site_id != source_resume.current_site_id
        || placement.target_site_id != target_resume.current_site_id
        || placement.credential_reservation != resume_rebind.credential_reservation
    {
        bail!("placement evidence contradicts the exact owner/site resume transition");
    }

    let source_request: crate::thread_lifecycle::SealedRootExecutionRequest =
        serde_json::from_value(source_capsule.sealed_invocation.clone())
            .context("decode source sealed invocation for remote transition")?;
    let expected_target = source_request.for_remote_worker_adoption_invocation(
        source_resume,
        target_resume,
        resume_rebind,
    )?;
    if serde_json::to_value(expected_target)? != target_capsule.sealed_invocation {
        bail!("target capsule invocation differs outside its typed remote rebind");
    }

    placement
        .project_rebind
        .validate_capsule_transition(&source_capsule.project_authority)?;
    let source_sessions = source_capsule.admitted_persistent_session_capsules()?;
    let target_sessions = target_capsule.admitted_persistent_session_capsules()?;
    if target_sessions != placement.target_persistent_session_capsules
        || source_sessions.keys().ne(target_sessions.keys())
    {
        bail!("placement persistent-session names or target capsules changed");
    }
    let mut source_programs = BTreeMap::new();
    for (name, source_hash) in &source_sessions {
        let source = load_persistent_capsule(cas, source_hash)?;
        let target_hash = target_sessions
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("target omitted persistent session `{name}`"))?;
        let target = load_persistent_capsule(cas, target_hash)?;
        if source.exact_program_hash != target.exact_program_hash {
            bail!("target persistent session `{name}` changed its exact program");
        }
        source_programs.insert(name.clone(), source.exact_program_hash);
    }
    if source_programs != placement.persistent_dependency_programs {
        bail!("placement persistent dependency program map is not the source capsule set");
    }
    Ok(())
}

fn load_persistent_capsule(
    cas: &lillux::CasStore,
    hash: &str,
) -> anyhow::Result<ryeos_state::objects::AdmittedPersistentSessionCapsule> {
    let value = cas
        .get_object(hash)?
        .ok_or_else(|| anyhow::anyhow!("persistent-session capsule is absent: {hash}"))?;
    let capsule =
        ryeos_state::objects::AdmittedPersistentSessionCapsule::from_current_value(&value)?;
    if capsule.content_hash()? != hash {
        bail!("persistent-session capsule content hash is not canonical: {hash}");
    }
    Ok(capsule)
}

impl CredentialGenerationReservation {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (label, value) in [
            ("credential profile", self.profile_id.as_str()),
            ("credential owner", self.owner_principal.as_str()),
            ("credential reservation", self.reservation_id.as_str()),
            (
                "credential upstream session",
                self.upstream_session_id.as_str(),
            ),
        ] {
            label_value(label, value)?;
        }
        if self.generation == 0 {
            bail!("credential reservation generation must be positive");
        }
        hash("credential subject contract", &self.subject_contract_digest)?;
        hash("credential subject", &self.subject_digest)
    }
}

impl ProjectAuthorityRebind {
    pub fn validate(&self) -> anyhow::Result<()> {
        hash("project route", &self.route_digest)?;
        label_value(
            "source stable project identity",
            &self.source_stable_project_identity,
        )?;
        label_value(
            "target stable project identity",
            &self.target_stable_project_identity,
        )?;
        if self.source_stable_project_identity == self.target_stable_project_identity {
            bail!("project rebind must name distinct directional endpoints");
        }
        hash(
            "source project candidate",
            &self.source_candidate_snapshot_hash,
        )?;
        hash("source project base", &self.source_base_snapshot_hash)?;
        if let Some(head) = &self.target_expected_head_hash {
            hash("target expected project head", head)?;
        }
        self.target_authority.validate()
    }

    fn validate_capsule_transition(
        &self,
        source: &ExecutionProjectAuthority,
    ) -> anyhow::Result<()> {
        use ryeos_state::objects::{PinnedProjectRealization, PinnedTerminalPublication};
        self.validate()?;
        source.validate()?;
        let ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity: source_identity,
            base_snapshot_hash: source_base,
            realization: PinnedProjectRealization::Cow { .. },
            environment: source_environment,
            capability_ceiling: source_capabilities,
            child_policy: source_child_policy,
            ..
        } = source
        else {
            bail!("cross-site worker handoff requires a pinned COW source project");
        };
        let ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity: target_identity,
            base_snapshot_hash: target_base,
            snapshot_hash: target_snapshot,
            realization:
                PinnedProjectRealization::Cow {
                    terminal_publication,
                },
            environment: target_environment,
            capability_ceiling: target_capabilities,
            child_policy: target_child_policy,
            ..
        } = &self.target_authority
        else {
            bail!("cross-site worker handoff requires a pinned COW target project");
        };
        if source_identity != &self.source_stable_project_identity
            || target_identity != &self.target_stable_project_identity
            || source_base != &self.source_base_snapshot_hash
            || target_base != source_base
            || target_snapshot != &self.source_candidate_snapshot_hash
            || source_capabilities != target_capabilities
            || source_child_policy != target_child_policy
            || !equivalent_remote_environment(source_environment, target_environment)
        {
            bail!("target project authority changed outside its directional endpoint rebind");
        }
        let expected_head = self.target_expected_head_hash.as_deref().ok_or_else(|| {
            anyhow::anyhow!("remote project rebind has no exact target HEAD fence")
        })?;
        let publication_expected = match terminal_publication {
            PinnedTerminalPublication::RetainCurrentHead { expected_hash, .. }
            | PinnedTerminalPublication::AdvanceHead { expected_hash, .. } => expected_hash,
            PinnedTerminalPublication::Discard | PinnedTerminalPublication::RetainResult => {
                bail!("remote target COW has no target project HEAD publication fence")
            }
        };
        if publication_expected != expected_head {
            bail!("target project publication authority contradicts its admitted HEAD fence");
        }
        Ok(())
    }
}

fn equivalent_remote_environment(
    source: &ryeos_state::objects::EnvironmentAuthority,
    target: &ryeos_state::objects::EnvironmentAuthority,
) -> bool {
    use ryeos_state::objects::EnvironmentAuthority;
    match (source, target) {
        (
            EnvironmentAuthority::ProjectOverlay {
                include_operator_vault: source_vault,
                name_authority: source_names,
                ..
            },
            EnvironmentAuthority::ProjectOverlay {
                include_operator_vault: target_vault,
                name_authority: target_names,
                ..
            },
        ) => source_vault == target_vault && source_names == target_names,
        _ => source == target,
    }
}

impl AccountingConservation {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.source_scope.is_some() != self.target_scope.is_some() {
            bail!("accounting handoff cannot create or discard an admitted scope");
        }
        if let Some(scope) = &self.source_scope {
            scope.validate()?;
        }
        if let Some(scope) = &self.target_scope {
            scope.validate()?;
        }
        if self.source_scope.is_none()
            && (self.source_financial_high_water != 0
                || self.source_charged_usd_nanos != 0
                || self.source_remaining_cap_usd_nanos.is_some()
                || self.target_cap_usd_nanos.is_some()
                || self.source_remaining_directive_cap_usd_nanos.is_some()
                || self.target_directive_cap_usd_nanos.is_some())
        {
            bail!("accounting-free handoff carries financial authority");
        }
        match (
            self.source_remaining_cap_usd_nanos,
            self.target_cap_usd_nanos,
        ) {
            (Some(source), Some(target)) if target <= source => {}
            (None, None) => {}
            _ => bail!("target accounting cap must be present and no larger than source remainder"),
        }
        let has_directive_scope = self
            .source_scope
            .as_ref()
            .and_then(|scope| scope.directive_budget_id.as_ref())
            .is_some();
        if has_directive_scope
            != self
                .target_scope
                .as_ref()
                .and_then(|scope| scope.directive_budget_id.as_ref())
                .is_some()
        {
            bail!("accounting handoff cannot create or discard a directive budget scope");
        }
        match (
            has_directive_scope,
            self.source_remaining_directive_cap_usd_nanos,
            self.target_directive_cap_usd_nanos,
        ) {
            (true, Some(source), Some(target)) if target <= source => {}
            (true, None, None) => {}
            (false, None, None) => {}
            _ => bail!(
                "target directive cap must be present and no larger than the source directive remainder"
            ),
        }
        Ok(())
    }
}

fn validate_common(
    operation_id: &str,
    owner_principal: &str,
    chain_root_id: &str,
    origin_site_id: &str,
    source_site_id: &str,
    target_site_id: &str,
    source_thread_id: &str,
    successor_thread_id: &str,
    source_head_hash: &str,
    source_event_hash: &str,
    checkpoint_hash: &str,
    target_capsule_hash: &str,
) -> anyhow::Result<()> {
    hash("handoff operation", operation_id)?;
    for (label, value) in [
        ("owner principal", owner_principal),
        ("chain root", chain_root_id),
        ("origin site", origin_site_id),
        ("source site", source_site_id),
        ("target site", target_site_id),
        ("source placement", source_thread_id),
        ("successor placement", successor_thread_id),
    ] {
        label_value(label, value)?;
    }
    if source_thread_id == successor_thread_id {
        bail!("handoff successor must differ from its source placement");
    }
    for (label, value) in [
        ("source chain head", source_head_hash),
        ("source last event", source_event_hash),
        ("checkpoint manifest", checkpoint_hash),
        ("target launch capsule", target_capsule_hash),
    ] {
        hash(label, value)?;
    }
    Ok(())
}

fn validate_dependency_maps(
    programs: &BTreeMap<String, String>,
    capsules: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    if programs.len() > 64 || programs.keys().ne(capsules.keys()) {
        bail!("target persistent-session capsules do not match named program dependencies");
    }
    for ((name, program), capsule) in programs.iter().zip(capsules.values()) {
        label_value("persistent dependency name", name)?;
        hash("persistent dependency program", program)?;
        hash("target persistent-session capsule", capsule)?;
    }
    Ok(())
}

fn hash(label: &str, value: &str) -> anyhow::Result<()> {
    ryeos_state::objects::thread_snapshot::validate_canonical_hash(label, value)
}

fn label_value(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 4096
        || value.trim() != value
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        bail!("{label} is not a bounded canonical label");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::contracts::{EffectivePrincipal, ExecutionHints, Principal, ProjectContext};
    use ryeos_state::objects::{
        ChildProjectAuthorityPolicy, EnvironmentAuthority, ExecutionLifecycleAuthority,
        PinnedProjectRealization, PinnedTerminalPublication,
    };

    fn accounting() -> AccountingConservation {
        AccountingConservation {
            source_scope: None,
            target_scope: None,
            source_financial_high_water: 4,
            source_charged_usd_nanos: 5,
            source_remaining_cap_usd_nanos: None,
            target_cap_usd_nanos: None,
            source_remaining_directive_cap_usd_nanos: None,
            target_directive_cap_usd_nanos: None,
        }
    }

    #[test]
    fn accounting_refuses_cap_inflation() {
        let mut value = accounting();
        value.source_scope = Some(AdmittedAccountingScope {
            budget_authority_site_id: "site:a".into(),
            ledger_epoch: 1,
            execution_budget_id: "budget:a".into(),
            directive_budget_id: None,
        });
        value.target_scope = Some(AdmittedAccountingScope {
            budget_authority_site_id: "site:b".into(),
            ledger_epoch: 1,
            execution_budget_id: "budget:b".into(),
            directive_budget_id: None,
        });
        value.source_remaining_cap_usd_nanos = Some(10);
        value.target_cap_usd_nanos = Some(11);
        assert!(value.validate().is_err());
        value.target_cap_usd_nanos = Some(10);
        assert!(value.validate().is_ok());
    }

    #[test]
    fn credential_subject_and_generation_are_both_fenced() {
        let mut reservation = CredentialGenerationReservation {
            profile_id: "profile-a".into(),
            owner_principal: "owner-a".into(),
            generation: 1,
            reservation_id: "reservation-a".into(),
            upstream_session_id: "upstream-a".into(),
            subject_contract_digest: "1".repeat(64),
            subject_digest: "2".repeat(64),
        };
        assert!(reservation.validate().is_ok());
        reservation.generation = 0;
        assert!(reservation.validate().is_err());
    }

    fn pinned_authority(
        site: &str,
        path: &str,
        base: &str,
        current: &str,
    ) -> ExecutionProjectAuthority {
        ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity: format!("{site}:{path}"),
            display_path: Some(PathBuf::from(path)),
            base_snapshot_hash: base.to_owned(),
            snapshot_hash: current.to_owned(),
            realization: PinnedProjectRealization::Cow {
                terminal_publication: PinnedTerminalPublication::RetainCurrentHead {
                    principal_key: "a".repeat(64),
                    project_hash: "b".repeat(64),
                    expected_hash: base.to_owned(),
                },
            },
            environment: EnvironmentAuthority::None,
            capability_ceiling: vec!["project.read".into(), "project.write".into()],
            child_policy: ChildProjectAuthorityPolicy::Inherit,
        }
    }

    fn source_resume() -> ResumeContext {
        let base = "1".repeat(64);
        let path = PathBuf::from("/source/project");
        ResumeContext {
            kind: "worker_execution".into(),
            item_ref: "worker_execution:test/session".into(),
            ref_bindings: BTreeMap::new(),
            launch_mode: "detached".into(),
            parameters: serde_json::json!({"credential_profile_id":"source-profile"}),
            project_context: ProjectContext::SnapshotHash { hash: base.clone() },
            project_authority: pinned_authority("site:a", "/source/project", &base, &base),
            lifecycle_authority: ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            stable_project_identity: Some(
                StableProjectIdentity::from_path(&path, "site:a").unwrap(),
            ),
            local_overlay_root: None,
            original_snapshot_hash: Some(base.clone()),
            original_pushed_head_ref: Some(OriginalPushedHeadRef {
                snapshot_hash: base,
                original_project_path: path,
            }),
            state_root: None,
            current_site_id: "site:a".into(),
            origin_site_id: "site:a".into(),
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "owner-a".into(),
                scopes: vec!["execute".into()],
            }),
            execution_hints: ExecutionHints::default(),
            effective_caps: vec!["project.read".into(), "project.write".into()],
            parent_delegation_caps: None,
            executor_ref: Some("native:worker-execution".into()),
            runtime_ref: Some("runtime:worker-execution-runtime".into()),
        }
    }

    fn remote_rebind() -> RemoteResumeContextRebind {
        let base = "1".repeat(64);
        let candidate = "2".repeat(64);
        let path = PathBuf::from("/target/project");
        RemoteResumeContextRebind {
            source_site_id: "site:a".into(),
            target_site_id: "site:b".into(),
            target_project_context: ProjectContext::SnapshotHash {
                hash: candidate.clone(),
            },
            target_project_authority: pinned_authority(
                "site:b",
                "/target/project",
                &base,
                &candidate,
            ),
            target_stable_project_identity: Some(
                StableProjectIdentity::from_path(&path, "site:b").unwrap(),
            ),
            target_local_overlay_root: None,
            target_original_snapshot_hash: Some(candidate.clone()),
            target_original_pushed_head_ref: Some(OriginalPushedHeadRef {
                snapshot_hash: candidate,
                original_project_path: path,
            }),
            target_state_root: None,
            source_credential_profile_id: "source-profile".into(),
            credential_reservation: CredentialGenerationReservation {
                profile_id: "target-profile".into(),
                owner_principal: "owner-a".into(),
                generation: 7,
                reservation_id: "handoff-reservation".into(),
                upstream_session_id: "upstream-session".into(),
                subject_contract_digest: "3".repeat(64),
                subject_digest: "4".repeat(64),
            },
        }
    }

    #[test]
    fn remote_resume_rebind_changes_only_typed_local_authorities() {
        let source = source_resume();
        let rebind = remote_rebind();
        let target = source.for_remote_worker_adoption(&rebind).unwrap();
        assert_eq!(target.current_site_id, "site:b");
        assert_eq!(
            target.parameters["credential_profile_id"],
            serde_json::json!("target-profile")
        );
        target
            .validate_remote_worker_adoption_from(&source, &rebind)
            .unwrap();

        let mut drifted = target;
        drifted.effective_caps.push("node.admin".into());
        assert!(
            drifted
                .validate_remote_worker_adoption_from(&source, &rebind)
                .is_err()
        );

        let project_rebind = ProjectAuthorityRebind {
            route_digest: "5".repeat(64),
            source_stable_project_identity: "site:a:/source/project".into(),
            target_stable_project_identity: "site:b:/target/project".into(),
            source_candidate_snapshot_hash: "2".repeat(64),
            source_base_snapshot_hash: "1".repeat(64),
            target_expected_head_hash: Some("1".repeat(64)),
            target_authority: rebind.target_project_authority.clone(),
        };
        project_rebind
            .validate_capsule_transition(&source.project_authority)
            .unwrap();
        let mut drifted_project = project_rebind;
        if let ExecutionProjectAuthority::PinnedGeneration {
            capability_ceiling, ..
        } = &mut drifted_project.target_authority
        {
            capability_ceiling.push("node.admin".into());
        }
        assert!(
            drifted_project
                .validate_capsule_transition(&source.project_authority)
                .is_err()
        );
    }
}
