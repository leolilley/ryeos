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
pub const WORKER_PLACEMENT_PREFLIGHT_POLICY: &str = "worker-placement-preflight-v1";
pub const WORKER_PLACEMENT_PREFLIGHT_CLAIM: &str = "eligible";
pub const WORKER_SESSION_HANDOFF_PREFLIGHT_OPERATION: &str = "worker_session_handoff_preflight";
pub const WORKER_SESSION_HANDOFF_OPERATION: &str = "worker_session_handoff";
pub const WORKER_PLACEMENT_PREFLIGHT_SERVICE: &str = "service:worker-placements/preflight";
pub const WORKER_PLACEMENT_PREPARE_SERVICE: &str = "service:worker-placements/prepare";
pub const WORKER_PLACEMENT_ADOPT_SERVICE: &str = "service:worker-placements/adopt";
pub const WORKER_PLACEMENT_ABORT_SERVICE: &str = "service:worker-placements/abort";

const PLACEMENT_EVIDENCE_SCHEMA: &str = "ryeos.worker_placement_admission.v2";
const PREFLIGHT_EVIDENCE_SCHEMA: &str = "ryeos.worker_placement_preflight.v2";
const PREFLIGHT_JOB_SCHEMA: &str = "ryeos.worker_session_handoff_preflight_job.v2";
const HANDOFF_JOB_SCHEMA: &str = "ryeos.worker_session_handoff_job.v3";
const HANDOFF_PROGRESS_SCHEMA: &str = "ryeos.worker_session_handoff_progress.v1";

/// Non-final target check performed while the source placement may still be
/// live. The launch metadata is transport data, not trusted authority: the
/// target reproduces its capsule and binds its canonical digest in the signed
/// receipt before using it for local preparation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementPreflightRequest {
    pub preflight_id: String,
    pub owner_principal: String,
    pub chain_root_id: String,
    pub origin_site_id: String,
    pub source_site_id: String,
    pub target_site_id: String,
    pub source_placement_thread_id: String,
    pub successor_placement_thread_id: String,
    pub source_chain_head_hash: String,
    pub source_last_event_hash: String,
    pub source_launch_capsule_hash: String,
    pub source_launch_metadata: serde_json::Value,
    pub source_launch_metadata_blob_hash: String,
    pub target_project_path: String,
    pub project_route_digest: String,
    pub target_credential_profile_id: String,
    pub upstream_session_id: String,
    pub credential_subject_contract_digest: String,
    pub credential_subject_digest: String,
    pub follow_delivery_reservation_attestation_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementPreflightResponse {
    pub preflight_id: String,
    pub preflight_attestation_hash: String,
    pub preflight_attestation: Attestation,
    pub evidence: WorkerPlacementPreflightEvidence,
}

/// Exact target-signed receipt for checks that do not require the final
/// checkpoint/candidate. This never grants chain-writer or process authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementPreflightEvidence {
    pub schema: String,
    pub operation_type: String,
    pub preflight_id: String,
    pub owner_principal: String,
    pub chain_root_id: String,
    pub origin_site_id: String,
    pub source_site_id: String,
    pub target_site_id: String,
    pub source_placement_thread_id: String,
    pub successor_placement_thread_id: String,
    pub source_chain_head_hash: String,
    pub source_last_event_hash: String,
    pub source_launch_capsule_hash: String,
    pub source_launch_metadata_blob_hash: String,
    pub outer_exact_program_hash: String,
    pub persistent_dependency_programs: BTreeMap<String, String>,
    pub target_persistent_session_capsules: BTreeMap<String, String>,
    pub target_execution_realization_hash: String,
    pub target_isolation_digest: String,
    pub target_project_path: String,
    pub project_route_digest: String,
    pub target_project_head_hash: String,
    pub target_credential_profile_id: String,
    pub target_credential_generation: u64,
    pub upstream_session_id: String,
    pub credential_subject_contract_digest: String,
    pub credential_subject_digest: String,
    pub follow_delivery_reservation_attestation_hash: Option<String>,
}

/// Immutable local sync-job body used to retain a preflight receipt and its
/// staged portable-program closure on each participating node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementPreflightJobOperation {
    pub schema: String,
    pub operation_type: String,
    pub role: WorkerHandoffJobRole,
    pub preflight_id: String,
    pub owner_principal: String,
    pub chain_root_id: String,
    pub origin_site_id: String,
    pub source_site_id: String,
    pub target_site_id: String,
    pub source_placement_thread_id: String,
    pub successor_placement_thread_id: String,
    pub source_chain_head_hash: String,
    pub source_last_event_hash: String,
    pub source_launch_capsule_hash: String,
    pub source_launch_metadata_blob_hash: String,
    pub peer_remote_name: String,
    pub target_project_path: String,
    pub project_route_digest: String,
    pub target_credential_profile_id: String,
    pub follow_delivery_reservation_attestation_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementPrepareRequest {
    pub preflight_id: String,
    pub preflight_attestation_hash: String,
    pub operation_id: String,
    pub chain_root_id: String,
    pub source_site_id: String,
    pub target_site_id: String,
    pub source_chain_head_hash: String,
    pub transfer_manifest_hash: String,
    pub target_project_path: String,
    pub project_route_digest: String,
    pub target_credential_profile_id: String,
    pub follow_delivery_reservation_attestation_hash: Option<String>,
    pub source_accounting_frontier: Option<crate::accounting_db::AccountingHandoffFrontier>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementPrepareResponse {
    pub operation_id: String,
    pub placement_attestation_hash: String,
    pub target_runtime_seed_hash: String,
    pub target_launch_capsule_hash: String,
    pub credential_reservation: CredentialGenerationReservation,
    pub placement: WorkerPlacementAdmissionEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementAdoptRequest {
    pub operation_id: String,
    pub chain_root_id: String,
    pub target_chain_head_hash: String,
    pub placement_attestation_hash: String,
    pub writer_grant_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementAdoptResponse {
    pub operation_id: String,
    pub chain_root_id: String,
    pub placement_thread_id: String,
    pub target_chain_head_hash: String,
    pub delivery: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementAbortRequest {
    pub operation_id: String,
    pub chain_root_id: String,
    pub abort_chain_head_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPlacementAbortResponse {
    pub operation_id: String,
    pub chain_root_id: String,
    pub disposition: String,
}

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
    AbortAuthorized,
    SourceCommitted,
    TargetAdopted,
    StateInstalled,
    ProcessAttached,
    Completed,
}

impl WorkerHandoffPhase {
    pub const ALL: [Self; 9] = [
        Self::Planned,
        Self::SourceExported,
        Self::TargetPrepared,
        Self::AbortAuthorized,
        Self::SourceCommitted,
        Self::TargetAdopted,
        Self::StateInstalled,
        Self::ProcessAttached,
        Self::Completed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::SourceExported => "source_exported",
            Self::TargetPrepared => "target_prepared",
            Self::AbortAuthorized => "abort_authorized",
            Self::SourceCommitted => "source_committed",
            Self::TargetAdopted => "target_adopted",
            Self::StateInstalled => "state_installed",
            Self::ProcessAttached => "process_attached",
            Self::Completed => "completed",
        }
    }

    /// Before the source cut, only the source placement may append. Abort is
    /// deliberately on this side of the cut because it preserves the source
    /// as the current placement.
    pub const fn source_is_only_authorized_writer(self) -> bool {
        matches!(
            self,
            Self::Planned | Self::SourceExported | Self::TargetPrepared | Self::AbortAuthorized
        )
    }

    /// At and after the source cut, source authority is permanently fenced and
    /// the exact successor grant is the sole append authority. The target may
    /// not exercise that grant until adoption verifies it.
    pub const fn successor_is_only_authorized_writer(self) -> bool {
        matches!(
            self,
            Self::SourceCommitted
                | Self::TargetAdopted
                | Self::StateInstalled
                | Self::ProcessAttached
                | Self::Completed
        )
    }

    pub const fn target_has_adopted_writer_grant(self) -> bool {
        matches!(
            self,
            Self::TargetAdopted | Self::StateInstalled | Self::ProcessAttached | Self::Completed
        )
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::collections::BTreeMap;
    use std::fmt;
    use std::io::Write;
    use std::str::FromStr;
    use std::sync::Mutex;

    use anyhow::{Context, Result};
    use serde::{Deserialize, Serialize};

    use super::WorkerHandoffPhase;

    macro_rules! handoff_crash_boundaries {
        ($($variant:ident => $name:literal),+ $(,)?) => {
            #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
            #[serde(rename_all = "snake_case")]
            pub enum HandoffCrashBoundary {
                $($variant),+
            }

            impl HandoffCrashBoundary {
                pub const ALL: &'static [Self] = &[$(Self::$variant),+];

                pub const fn as_str(self) -> &'static str {
                    match self {
                        $(Self::$variant => $name),+
                    }
                }
            }

            impl FromStr for HandoffCrashBoundary {
                type Err = anyhow::Error;

                fn from_str(value: &str) -> Result<Self> {
                    match value {
                        $($name => Ok(Self::$variant)),+,
                        _ => anyhow::bail!("unknown handoff crash boundary `{value}`"),
                    }
                }
            }
        };
    }

    handoff_crash_boundaries! {
        SourceBeforeExportPublication => "source_before_export_publication",
        SourceExportPublished => "source_export_published",
        SourceBeforePreparedEvidenceProjection => "source_before_prepared_evidence_projection",
        SourcePreparedEvidenceProjected => "source_prepared_evidence_projected",
        SourceBeforeWriterCut => "source_before_writer_cut",
        SourceWriterCutPublished => "source_writer_cut_published",
        SourceCommitProjected => "source_commit_projected",
        SourceBeforeAbortPublication => "source_before_abort_publication",
        SourceAbortPublished => "source_abort_published",
        SourceAbortProjected => "source_abort_projected",
        SourceBeforeCompletion => "source_before_completion",
        SourceCompletedBeforeResponse => "source_completed_before_response",
        TargetBeforePreparationPublication => "target_before_preparation_publication",
        TargetPreparationPublished => "target_preparation_published",
        TargetBeforeAbortEvidenceStage => "target_before_abort_evidence_stage",
        TargetAbortEvidenceStaged => "target_abort_evidence_staged",
        TargetAbortEvidenceVerified => "target_abort_evidence_verified",
        TargetAbortReservationReleased => "target_abort_reservation_released",
        TargetAbortCompletedBeforeResponse => "target_abort_completed_before_response",
        TargetBeforeSourceCommitEvidenceStage => "target_before_source_commit_evidence_stage",
        TargetSourceCommitEvidenceStaged => "target_source_commit_evidence_staged",
        TargetSourceCommitEvidenceVerified => "target_source_commit_evidence_verified",
        TargetBeforeAdoptionPublication => "target_before_adoption_publication",
        TargetAdoptionPublished => "target_adoption_published",
        TargetAdoptionProjected => "target_adoption_projected",
        TargetBeforeStateInstall => "target_before_state_install",
        TargetStateInstalled => "target_state_installed",
        TargetStateInstallProjected => "target_state_install_projected",
        TargetProcessAttachmentObserved => "target_process_attachment_observed",
        TargetProcessAttachmentProjected => "target_process_attachment_projected",
        TargetBeforeCompletion => "target_before_completion",
        TargetCompletedBeforeResponse => "target_completed_before_response",
    }

    impl fmt::Display for HandoffCrashBoundary {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.as_str())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum HandoffNode {
        Source,
        Target,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CrashCutPosition {
        Before,
        After,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum HandoffProtocolMilestone {
        SourceExportPublication,
        SourcePreparedEvidenceProjection,
        SourceWriterCutPublication,
        SourceCommitProjection,
        SourceAbortPublication,
        SourceAbortProjection,
        SourceCompletion,
        TargetPreparationPublication,
        TargetAbortEvidenceStage,
        TargetAbortAuthorityVerification,
        TargetCredentialReservationRelease,
        TargetAbortCompletion,
        TargetSourceCommitEvidenceStage,
        TargetSourceCommitAuthorityVerification,
        TargetAdoptionPublication,
        TargetAdoptionProjection,
        TargetPortableStateInstall,
        TargetStateInstallProjection,
        TargetProcessAttachment,
        TargetProcessAttachmentProjection,
        TargetCompletion,
    }

    /// One exact instrumentation seam. This inventory is deliberately
    /// separate from the acceptance cases: the same seam can be exercised by
    /// multiple durable starting states and therefore cannot itself be the
    /// recovery oracle.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    pub struct HandoffCrashBoundarySpec {
        pub boundary: HandoffCrashBoundary,
        pub interrupted_node: HandoffNode,
        pub milestone: HandoffProtocolMilestone,
        pub position: CrashCutPosition,
    }

    macro_rules! boundary_spec {
        ($boundary:ident, $node:ident, $milestone:ident, $position:ident) => {
            HandoffCrashBoundarySpec {
                boundary: HandoffCrashBoundary::$boundary,
                interrupted_node: HandoffNode::$node,
                milestone: HandoffProtocolMilestone::$milestone,
                position: CrashCutPosition::$position,
            }
        };
    }

    pub const HANDOFF_CRASH_BOUNDARY_SPECS: &[HandoffCrashBoundarySpec] = &[
        boundary_spec!(
            SourceBeforeExportPublication,
            Source,
            SourceExportPublication,
            Before
        ),
        boundary_spec!(
            SourceExportPublished,
            Source,
            SourceExportPublication,
            After
        ),
        boundary_spec!(
            SourceBeforePreparedEvidenceProjection,
            Source,
            SourcePreparedEvidenceProjection,
            Before
        ),
        boundary_spec!(
            SourcePreparedEvidenceProjected,
            Source,
            SourcePreparedEvidenceProjection,
            After
        ),
        boundary_spec!(
            SourceBeforeWriterCut,
            Source,
            SourceWriterCutPublication,
            Before
        ),
        boundary_spec!(
            SourceWriterCutPublished,
            Source,
            SourceWriterCutPublication,
            After
        ),
        boundary_spec!(SourceCommitProjected, Source, SourceCommitProjection, After),
        boundary_spec!(
            SourceBeforeAbortPublication,
            Source,
            SourceAbortPublication,
            Before
        ),
        boundary_spec!(SourceAbortPublished, Source, SourceAbortPublication, After),
        boundary_spec!(SourceAbortProjected, Source, SourceAbortProjection, After),
        boundary_spec!(SourceBeforeCompletion, Source, SourceCompletion, Before),
        boundary_spec!(
            SourceCompletedBeforeResponse,
            Source,
            SourceCompletion,
            After
        ),
        boundary_spec!(
            TargetBeforePreparationPublication,
            Target,
            TargetPreparationPublication,
            Before
        ),
        boundary_spec!(
            TargetPreparationPublished,
            Target,
            TargetPreparationPublication,
            After
        ),
        boundary_spec!(
            TargetBeforeAbortEvidenceStage,
            Target,
            TargetAbortEvidenceStage,
            Before
        ),
        boundary_spec!(
            TargetAbortEvidenceStaged,
            Target,
            TargetAbortEvidenceStage,
            After
        ),
        boundary_spec!(
            TargetAbortEvidenceVerified,
            Target,
            TargetAbortAuthorityVerification,
            After
        ),
        boundary_spec!(
            TargetAbortReservationReleased,
            Target,
            TargetCredentialReservationRelease,
            After
        ),
        boundary_spec!(
            TargetAbortCompletedBeforeResponse,
            Target,
            TargetAbortCompletion,
            After
        ),
        boundary_spec!(
            TargetBeforeSourceCommitEvidenceStage,
            Target,
            TargetSourceCommitEvidenceStage,
            Before
        ),
        boundary_spec!(
            TargetSourceCommitEvidenceStaged,
            Target,
            TargetSourceCommitEvidenceStage,
            After
        ),
        boundary_spec!(
            TargetSourceCommitEvidenceVerified,
            Target,
            TargetSourceCommitAuthorityVerification,
            After
        ),
        boundary_spec!(
            TargetBeforeAdoptionPublication,
            Target,
            TargetAdoptionPublication,
            Before
        ),
        boundary_spec!(
            TargetAdoptionPublished,
            Target,
            TargetAdoptionPublication,
            After
        ),
        boundary_spec!(
            TargetAdoptionProjected,
            Target,
            TargetAdoptionProjection,
            After
        ),
        boundary_spec!(
            TargetBeforeStateInstall,
            Target,
            TargetPortableStateInstall,
            Before
        ),
        boundary_spec!(
            TargetStateInstalled,
            Target,
            TargetPortableStateInstall,
            After
        ),
        boundary_spec!(
            TargetStateInstallProjected,
            Target,
            TargetStateInstallProjection,
            After
        ),
        boundary_spec!(
            TargetProcessAttachmentObserved,
            Target,
            TargetProcessAttachment,
            After
        ),
        boundary_spec!(
            TargetProcessAttachmentProjected,
            Target,
            TargetProcessAttachmentProjection,
            After
        ),
        boundary_spec!(TargetBeforeCompletion, Target, TargetCompletion, Before),
        boundary_spec!(
            TargetCompletedBeforeResponse,
            Target,
            TargetCompletion,
            After
        ),
    ];

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RequestOutcomeAtCut {
        FailedBeforeResponse,
        AmbiguousResponse,
        RecoveryInterrupted,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RecoveryTrigger {
        RetryOriginalRequest,
        RestartSource,
        RestartTargetThenRetrySource,
        RestartTargetThenSourceRecovery,
        ObserveOnly,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AppendAuthorityLane {
        SourcePlacement,
        ExactSuccessorGrant,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ExpectedHeadSigner {
        SourceNode,
        TargetNode,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum DurableJobState {
        Absent,
        Planned,
        Running,
        Retryable,
        Completed,
        Cancelled,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum DurableJobPhase {
        Absent,
        Planned,
        SourceExported,
        TargetPrepare,
        PlacementAdmission,
        TargetPrepared,
        AbortAuthorized,
        TargetAbort,
        SourceCommitted,
        TargetAdopt,
        TargetAdopted,
        StateInstalled,
        ProcessAttached,
        Completed,
        Aborted,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case", tag = "kind", content = "phase")]
    pub enum DurableJobResult {
        Absent,
        Progress(WorkerHandoffPhase),
        AdoptionReceipt,
        AbortReceipt,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    pub struct DurableJobExpectation {
        pub state: DurableJobState,
        pub phase: DurableJobPhase,
        pub result: DurableJobResult,
        pub active_attempt: bool,
    }

    const fn absent_job() -> DurableJobExpectation {
        DurableJobExpectation {
            state: DurableJobState::Absent,
            phase: DurableJobPhase::Absent,
            result: DurableJobResult::Absent,
            active_attempt: false,
        }
    }

    const fn progress_job(
        state: DurableJobState,
        phase: DurableJobPhase,
        progress: WorkerHandoffPhase,
        active_attempt: bool,
    ) -> DurableJobExpectation {
        DurableJobExpectation {
            state,
            phase,
            result: DurableJobResult::Progress(progress),
            active_attempt,
        }
    }

    const fn no_result_job(
        state: DurableJobState,
        phase: DurableJobPhase,
        active_attempt: bool,
    ) -> DurableJobExpectation {
        DurableJobExpectation {
            state,
            phase,
            result: DurableJobResult::Absent,
            active_attempt,
        }
    }

    const fn receipt_job(
        state: DurableJobState,
        phase: DurableJobPhase,
        result: DurableJobResult,
    ) -> DurableJobExpectation {
        DurableJobExpectation {
            state,
            phase,
            result,
            active_attempt: false,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SourcePlacementState {
        CurrentWriter,
        AbortRecordedCurrentWriter,
        Fenced,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SuccessorPlacementState {
        Absent,
        PreparedOnly,
        AuthorizedUnadopted,
        AdoptedUnattached,
        Attached,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CredentialDisposition {
        NotReserved,
        Reserved,
        Released,
        Consumed,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum StagingRootDisposition {
        None,
        ActiveJobOwned,
        TerminalJobOwned,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum PortableStateDisposition {
        Absent,
        Installed,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum WorkspaceDisposition {
        None,
        EphemeralPreparation,
        PreparedReconstructible,
        Attached,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ProcessDisposition {
        None,
        ExactTargetAttached,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    pub struct HandoffDurableSnapshot {
        pub source_phase: Option<WorkerHandoffPhase>,
        pub target_phase: Option<WorkerHandoffPhase>,
        pub source_job: DurableJobExpectation,
        pub target_job: DurableJobExpectation,
        pub append_authority: AppendAuthorityLane,
        pub head_signer: ExpectedHeadSigner,
        pub source_placement: SourcePlacementState,
        pub successor_placement: SuccessorPlacementState,
        pub target_credential: CredentialDisposition,
        pub source_staging_roots: StagingRootDisposition,
        pub target_staging_roots: StagingRootDisposition,
        pub portable_state: PortableStateDisposition,
        pub workspace: WorkspaceDisposition,
        pub process: ProcessDisposition,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RetryDisposition {
        ReusesExactOperation,
        ResumesExactOperation,
        ObservesCompletedReceipt,
        RejectedByAbortAuthority,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum OperatorOutcome {
        Completed,
        AbortedSourceContinues,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    pub struct HandoffAcceptanceCase {
        pub case_id: &'static str,
        pub boundary: HandoffCrashBoundary,
        pub interrupted_node: HandoffNode,
        pub request_outcome_at_cut: RequestOutcomeAtCut,
        pub recovery_trigger: RecoveryTrigger,
        pub at_cut: HandoffDurableSnapshot,
        pub after_recovery: HandoffDurableSnapshot,
        pub retry_disposition: RetryDisposition,
        pub operator_outcome: OperatorOutcome,
    }

    macro_rules! acceptance_case {
        ($id:literal, $boundary:ident, $node:ident, $request:ident, $recovery:ident, $cut:expr, $after:expr, $retry:ident, $operator:ident) => {
            HandoffAcceptanceCase {
                case_id: $id,
                boundary: HandoffCrashBoundary::$boundary,
                interrupted_node: HandoffNode::$node,
                request_outcome_at_cut: RequestOutcomeAtCut::$request,
                recovery_trigger: RecoveryTrigger::$recovery,
                at_cut: $cut,
                after_recovery: $after,
                retry_disposition: RetryDisposition::$retry,
                operator_outcome: OperatorOutcome::$operator,
            }
        };
    }

    macro_rules! snapshot {
        (
            $source_phase:expr,
            $target_phase:expr,
            $source_job:expr,
            $target_job:expr,
            $authority:ident,
            $signer:ident,
            $source_placement:ident,
            $successor_placement:ident,
            $credential:ident,
            $source_roots:ident,
            $target_roots:ident,
            $portable_state:ident,
            $workspace:ident,
            $process:ident
        ) => {
            HandoffDurableSnapshot {
                source_phase: $source_phase,
                target_phase: $target_phase,
                source_job: $source_job,
                target_job: $target_job,
                append_authority: AppendAuthorityLane::$authority,
                head_signer: ExpectedHeadSigner::$signer,
                source_placement: SourcePlacementState::$source_placement,
                successor_placement: SuccessorPlacementState::$successor_placement,
                target_credential: CredentialDisposition::$credential,
                source_staging_roots: StagingRootDisposition::$source_roots,
                target_staging_roots: StagingRootDisposition::$target_roots,
                portable_state: PortableStateDisposition::$portable_state,
                workspace: WorkspaceDisposition::$workspace,
                process: ProcessDisposition::$process,
            }
        };
    }

    const RECOVERED_COMPLETED: HandoffDurableSnapshot = snapshot!(
        Some(WorkerHandoffPhase::Completed),
        Some(WorkerHandoffPhase::Completed),
        receipt_job(
            DurableJobState::Completed,
            DurableJobPhase::Completed,
            DurableJobResult::AdoptionReceipt,
        ),
        receipt_job(
            DurableJobState::Completed,
            DurableJobPhase::Completed,
            DurableJobResult::AdoptionReceipt,
        ),
        ExactSuccessorGrant,
        TargetNode,
        Fenced,
        Attached,
        Consumed,
        TerminalJobOwned,
        TerminalJobOwned,
        Installed,
        Attached,
        ExactTargetAttached
    );

    const RECOVERED_ABORTED_WITH_TARGET: HandoffDurableSnapshot = snapshot!(
        Some(WorkerHandoffPhase::AbortAuthorized),
        Some(WorkerHandoffPhase::AbortAuthorized),
        receipt_job(
            DurableJobState::Cancelled,
            DurableJobPhase::Aborted,
            DurableJobResult::AbortReceipt,
        ),
        receipt_job(
            DurableJobState::Cancelled,
            DurableJobPhase::Aborted,
            DurableJobResult::AbortReceipt,
        ),
        SourcePlacement,
        SourceNode,
        AbortRecordedCurrentWriter,
        Absent,
        Released,
        TerminalJobOwned,
        TerminalJobOwned,
        Absent,
        None,
        None
    );

    const RECOVERED_ABORTED_NO_TARGET: HandoffDurableSnapshot = snapshot!(
        Some(WorkerHandoffPhase::AbortAuthorized),
        None,
        receipt_job(
            DurableJobState::Cancelled,
            DurableJobPhase::Aborted,
            DurableJobResult::AbortReceipt,
        ),
        absent_job(),
        SourcePlacement,
        SourceNode,
        AbortRecordedCurrentWriter,
        Absent,
        NotReserved,
        TerminalJobOwned,
        None,
        Absent,
        None,
        None
    );

    const CUT_SOURCE_BEFORE_EXPORT: HandoffDurableSnapshot = snapshot!(
        Some(WorkerHandoffPhase::Planned),
        None,
        absent_job(),
        absent_job(),
        SourcePlacement,
        SourceNode,
        CurrentWriter,
        Absent,
        NotReserved,
        None,
        None,
        Absent,
        None,
        None
    );

    const CUT_SOURCE_EXPORTED: HandoffDurableSnapshot = snapshot!(
        Some(WorkerHandoffPhase::SourceExported),
        None,
        progress_job(
            DurableJobState::Running,
            DurableJobPhase::SourceExported,
            WorkerHandoffPhase::SourceExported,
            false,
        ),
        absent_job(),
        SourcePlacement,
        SourceNode,
        CurrentWriter,
        Absent,
        NotReserved,
        ActiveJobOwned,
        None,
        Absent,
        None,
        None
    );

    const CUT_TARGET_PREPARATION_BEFORE_PUBLICATION: HandoffDurableSnapshot = snapshot!(
        Some(WorkerHandoffPhase::SourceExported),
        Some(WorkerHandoffPhase::Planned),
        progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetPrepare,
            WorkerHandoffPhase::SourceExported,
            true,
        ),
        no_result_job(
            DurableJobState::Running,
            DurableJobPhase::PlacementAdmission,
            true,
        ),
        SourcePlacement,
        SourceNode,
        CurrentWriter,
        PreparedOnly,
        Reserved,
        ActiveJobOwned,
        ActiveJobOwned,
        Absent,
        EphemeralPreparation,
        None
    );

    const CUT_TARGET_PREPARATION_PUBLISHED: HandoffDurableSnapshot = snapshot!(
        Some(WorkerHandoffPhase::SourceExported),
        Some(WorkerHandoffPhase::TargetPrepared),
        progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetPrepare,
            WorkerHandoffPhase::SourceExported,
            true,
        ),
        progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetPrepared,
            WorkerHandoffPhase::TargetPrepared,
            true,
        ),
        SourcePlacement,
        SourceNode,
        CurrentWriter,
        PreparedOnly,
        Reserved,
        ActiveJobOwned,
        ActiveJobOwned,
        Absent,
        PreparedReconstructible,
        None
    );

    const CUT_SOURCE_BEFORE_PREPARED_PROJECTION: HandoffDurableSnapshot = snapshot!(
        Some(WorkerHandoffPhase::SourceExported),
        Some(WorkerHandoffPhase::TargetPrepared),
        progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetPrepare,
            WorkerHandoffPhase::SourceExported,
            true,
        ),
        progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetPrepared,
            WorkerHandoffPhase::TargetPrepared,
            false,
        ),
        SourcePlacement,
        SourceNode,
        CurrentWriter,
        PreparedOnly,
        Reserved,
        ActiveJobOwned,
        ActiveJobOwned,
        Absent,
        PreparedReconstructible,
        None
    );

    const CUT_SOURCE_PREPARED_PROJECTED_ACTIVE: HandoffDurableSnapshot = snapshot!(
        Some(WorkerHandoffPhase::TargetPrepared),
        Some(WorkerHandoffPhase::TargetPrepared),
        progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetPrepared,
            WorkerHandoffPhase::TargetPrepared,
            true,
        ),
        progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetPrepared,
            WorkerHandoffPhase::TargetPrepared,
            false,
        ),
        SourcePlacement,
        SourceNode,
        CurrentWriter,
        PreparedOnly,
        Reserved,
        ActiveJobOwned,
        ActiveJobOwned,
        Absent,
        PreparedReconstructible,
        None
    );

    const CUT_SOURCE_PREPARED_IDLE: HandoffDurableSnapshot = HandoffDurableSnapshot {
        source_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetPrepared,
            WorkerHandoffPhase::TargetPrepared,
            false,
        ),
        ..CUT_SOURCE_PREPARED_PROJECTED_ACTIVE
    };

    const CUT_SOURCE_WRITER_CUT_PUBLISHED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        append_authority: AppendAuthorityLane::ExactSuccessorGrant,
        source_placement: SourcePlacementState::Fenced,
        successor_placement: SuccessorPlacementState::AuthorizedUnadopted,
        ..CUT_SOURCE_PREPARED_IDLE
    };

    const CUT_SOURCE_COMMIT_PROJECTED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        source_phase: Some(WorkerHandoffPhase::SourceCommitted),
        source_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::SourceCommitted,
            WorkerHandoffPhase::SourceCommitted,
            false,
        ),
        ..CUT_SOURCE_WRITER_CUT_PUBLISHED
    };

    const CUT_SOURCE_ABORT_BEFORE_NO_TARGET: HandoffDurableSnapshot = CUT_SOURCE_EXPORTED;

    const CUT_SOURCE_ABORT_PUBLISHED_NO_TARGET: HandoffDurableSnapshot = HandoffDurableSnapshot {
        source_placement: SourcePlacementState::AbortRecordedCurrentWriter,
        ..CUT_SOURCE_EXPORTED
    };

    const CUT_SOURCE_ABORT_PROJECTED_NO_TARGET: HandoffDurableSnapshot = HandoffDurableSnapshot {
        source_phase: Some(WorkerHandoffPhase::AbortAuthorized),
        source_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::AbortAuthorized,
            WorkerHandoffPhase::AbortAuthorized,
            false,
        ),
        source_placement: SourcePlacementState::AbortRecordedCurrentWriter,
        ..CUT_SOURCE_EXPORTED
    };

    const CUT_SOURCE_ABORT_BEFORE_WITH_TARGET: HandoffDurableSnapshot = CUT_SOURCE_PREPARED_IDLE;

    const CUT_SOURCE_ABORT_PUBLISHED_WITH_TARGET: HandoffDurableSnapshot = HandoffDurableSnapshot {
        source_placement: SourcePlacementState::AbortRecordedCurrentWriter,
        ..CUT_SOURCE_PREPARED_IDLE
    };

    const CUT_SOURCE_ABORT_PROJECTED_WITH_TARGET: HandoffDurableSnapshot = HandoffDurableSnapshot {
        source_phase: Some(WorkerHandoffPhase::AbortAuthorized),
        source_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::AbortAuthorized,
            WorkerHandoffPhase::AbortAuthorized,
            false,
        ),
        source_placement: SourcePlacementState::AbortRecordedCurrentWriter,
        ..CUT_SOURCE_PREPARED_IDLE
    };

    const CUT_TARGET_ABORT_BEFORE_STAGE: HandoffDurableSnapshot = HandoffDurableSnapshot {
        source_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetAbort,
            WorkerHandoffPhase::AbortAuthorized,
            true,
        ),
        ..CUT_SOURCE_ABORT_PROJECTED_WITH_TARGET
    };

    const CUT_TARGET_ABORT_STAGED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        target_phase: Some(WorkerHandoffPhase::AbortAuthorized),
        target_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::AbortAuthorized,
            WorkerHandoffPhase::AbortAuthorized,
            false,
        ),
        ..CUT_TARGET_ABORT_BEFORE_STAGE
    };

    const CUT_TARGET_ABORT_RESERVATION_RELEASED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        target_credential: CredentialDisposition::Released,
        ..CUT_TARGET_ABORT_STAGED
    };

    const CUT_TARGET_ABORT_COMPLETED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        target_job: receipt_job(
            DurableJobState::Cancelled,
            DurableJobPhase::Aborted,
            DurableJobResult::AbortReceipt,
        ),
        successor_placement: SuccessorPlacementState::Absent,
        target_credential: CredentialDisposition::Released,
        target_staging_roots: StagingRootDisposition::TerminalJobOwned,
        workspace: WorkspaceDisposition::None,
        ..CUT_TARGET_ABORT_STAGED
    };

    const CUT_TARGET_BEFORE_SOURCE_COMMIT_STAGE: HandoffDurableSnapshot = HandoffDurableSnapshot {
        source_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetAdopt,
            WorkerHandoffPhase::SourceCommitted,
            true,
        ),
        target_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetAdopt,
            WorkerHandoffPhase::TargetPrepared,
            true,
        ),
        ..CUT_SOURCE_COMMIT_PROJECTED
    };

    const CUT_TARGET_SOURCE_COMMIT_STAGED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        target_phase: Some(WorkerHandoffPhase::SourceCommitted),
        target_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::SourceCommitted,
            WorkerHandoffPhase::SourceCommitted,
            true,
        ),
        ..CUT_TARGET_BEFORE_SOURCE_COMMIT_STAGE
    };

    const CUT_TARGET_ADOPTION_PUBLISHED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        head_signer: ExpectedHeadSigner::TargetNode,
        successor_placement: SuccessorPlacementState::AdoptedUnattached,
        ..CUT_TARGET_SOURCE_COMMIT_STAGED
    };

    const CUT_TARGET_ADOPTION_PROJECTED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        target_phase: Some(WorkerHandoffPhase::TargetAdopted),
        target_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::TargetAdopted,
            WorkerHandoffPhase::TargetAdopted,
            true,
        ),
        ..CUT_TARGET_ADOPTION_PUBLISHED
    };

    const CUT_TARGET_STATE_INSTALLED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        portable_state: PortableStateDisposition::Installed,
        ..CUT_TARGET_ADOPTION_PROJECTED
    };

    const CUT_TARGET_STATE_PROJECTED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        target_phase: Some(WorkerHandoffPhase::StateInstalled),
        target_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::StateInstalled,
            WorkerHandoffPhase::StateInstalled,
            true,
        ),
        ..CUT_TARGET_STATE_INSTALLED
    };

    const CUT_TARGET_ATTACHMENT_OBSERVED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        successor_placement: SuccessorPlacementState::Attached,
        target_credential: CredentialDisposition::Consumed,
        workspace: WorkspaceDisposition::Attached,
        process: ProcessDisposition::ExactTargetAttached,
        ..CUT_TARGET_STATE_PROJECTED
    };

    const CUT_TARGET_ATTACHMENT_PROJECTED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        target_phase: Some(WorkerHandoffPhase::ProcessAttached),
        target_job: progress_job(
            DurableJobState::Running,
            DurableJobPhase::ProcessAttached,
            WorkerHandoffPhase::ProcessAttached,
            true,
        ),
        ..CUT_TARGET_ATTACHMENT_OBSERVED
    };

    const CUT_TARGET_COMPLETED: HandoffDurableSnapshot = HandoffDurableSnapshot {
        target_phase: Some(WorkerHandoffPhase::Completed),
        target_job: receipt_job(
            DurableJobState::Completed,
            DurableJobPhase::Completed,
            DurableJobResult::AdoptionReceipt,
        ),
        target_staging_roots: StagingRootDisposition::TerminalJobOwned,
        ..CUT_TARGET_ATTACHMENT_PROJECTED
    };

    const CUT_SOURCE_COMPLETED: HandoffDurableSnapshot = RECOVERED_COMPLETED;

    /// Executable recovery oracle. Several rows may select the same crash seam
    /// when distinct durable starting states require distinct assertions.
    pub const HANDOFF_ACCEPTANCE_MATRIX: &[HandoffAcceptanceCase] = &[
        acceptance_case!(
            "source_before_export",
            SourceBeforeExportPublication,
            Source,
            FailedBeforeResponse,
            RetryOriginalRequest,
            CUT_SOURCE_BEFORE_EXPORT,
            RECOVERED_COMPLETED,
            ReusesExactOperation,
            Completed
        ),
        acceptance_case!(
            "source_export_published",
            SourceExportPublished,
            Source,
            AmbiguousResponse,
            RestartSource,
            CUT_SOURCE_EXPORTED,
            RECOVERED_ABORTED_NO_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "source_before_prepared_projection",
            SourceBeforePreparedEvidenceProjection,
            Source,
            AmbiguousResponse,
            RestartSource,
            CUT_SOURCE_BEFORE_PREPARED_PROJECTION,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "source_prepared_projected",
            SourcePreparedEvidenceProjected,
            Source,
            AmbiguousResponse,
            RestartSource,
            CUT_SOURCE_PREPARED_PROJECTED_ACTIVE,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "source_before_writer_cut",
            SourceBeforeWriterCut,
            Source,
            AmbiguousResponse,
            RestartSource,
            CUT_SOURCE_PREPARED_IDLE,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "source_writer_cut_published",
            SourceWriterCutPublished,
            Source,
            AmbiguousResponse,
            RestartSource,
            CUT_SOURCE_WRITER_CUT_PUBLISHED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "source_commit_projected",
            SourceCommitProjected,
            Source,
            AmbiguousResponse,
            RestartSource,
            CUT_SOURCE_COMMIT_PROJECTED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "source_abort_before_no_target",
            SourceBeforeAbortPublication,
            Source,
            RecoveryInterrupted,
            RestartSource,
            CUT_SOURCE_ABORT_BEFORE_NO_TARGET,
            RECOVERED_ABORTED_NO_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "source_abort_before_with_target",
            SourceBeforeAbortPublication,
            Source,
            RecoveryInterrupted,
            RestartSource,
            CUT_SOURCE_ABORT_BEFORE_WITH_TARGET,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "source_abort_published_no_target",
            SourceAbortPublished,
            Source,
            RecoveryInterrupted,
            RestartSource,
            CUT_SOURCE_ABORT_PUBLISHED_NO_TARGET,
            RECOVERED_ABORTED_NO_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "source_abort_published_with_target",
            SourceAbortPublished,
            Source,
            RecoveryInterrupted,
            RestartSource,
            CUT_SOURCE_ABORT_PUBLISHED_WITH_TARGET,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "source_abort_projected_no_target",
            SourceAbortProjected,
            Source,
            RecoveryInterrupted,
            RestartSource,
            CUT_SOURCE_ABORT_PROJECTED_NO_TARGET,
            RECOVERED_ABORTED_NO_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "source_abort_projected_with_target",
            SourceAbortProjected,
            Source,
            RecoveryInterrupted,
            RestartSource,
            CUT_SOURCE_ABORT_PROJECTED_WITH_TARGET,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "source_before_completion",
            SourceBeforeCompletion,
            Source,
            AmbiguousResponse,
            RestartSource,
            CUT_TARGET_COMPLETED,
            RECOVERED_COMPLETED,
            ObservesCompletedReceipt,
            Completed
        ),
        acceptance_case!(
            "source_completed_before_response",
            SourceCompletedBeforeResponse,
            Source,
            AmbiguousResponse,
            ObserveOnly,
            CUT_SOURCE_COMPLETED,
            RECOVERED_COMPLETED,
            ObservesCompletedReceipt,
            Completed
        ),
        acceptance_case!(
            "target_before_preparation_publication",
            TargetBeforePreparationPublication,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_PREPARATION_BEFORE_PUBLICATION,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_preparation_published",
            TargetPreparationPublished,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_PREPARATION_PUBLISHED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_before_abort_stage",
            TargetBeforeAbortEvidenceStage,
            Target,
            RecoveryInterrupted,
            RestartTargetThenSourceRecovery,
            CUT_TARGET_ABORT_BEFORE_STAGE,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "target_abort_staged",
            TargetAbortEvidenceStaged,
            Target,
            RecoveryInterrupted,
            RestartTargetThenSourceRecovery,
            CUT_TARGET_ABORT_STAGED,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "target_abort_verified",
            TargetAbortEvidenceVerified,
            Target,
            RecoveryInterrupted,
            RestartTargetThenSourceRecovery,
            CUT_TARGET_ABORT_STAGED,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "target_abort_reservation_released",
            TargetAbortReservationReleased,
            Target,
            RecoveryInterrupted,
            RestartTargetThenSourceRecovery,
            CUT_TARGET_ABORT_RESERVATION_RELEASED,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "target_abort_completed",
            TargetAbortCompletedBeforeResponse,
            Target,
            RecoveryInterrupted,
            RestartTargetThenSourceRecovery,
            CUT_TARGET_ABORT_COMPLETED,
            RECOVERED_ABORTED_WITH_TARGET,
            RejectedByAbortAuthority,
            AbortedSourceContinues
        ),
        acceptance_case!(
            "target_before_source_commit_stage",
            TargetBeforeSourceCommitEvidenceStage,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_BEFORE_SOURCE_COMMIT_STAGE,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_source_commit_staged",
            TargetSourceCommitEvidenceStaged,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_SOURCE_COMMIT_STAGED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_source_commit_verified",
            TargetSourceCommitEvidenceVerified,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_SOURCE_COMMIT_STAGED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_before_adoption_publication",
            TargetBeforeAdoptionPublication,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_SOURCE_COMMIT_STAGED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_adoption_published",
            TargetAdoptionPublished,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_ADOPTION_PUBLISHED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_adoption_projected",
            TargetAdoptionProjected,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_ADOPTION_PROJECTED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_before_state_install",
            TargetBeforeStateInstall,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_ADOPTION_PROJECTED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_state_installed",
            TargetStateInstalled,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_STATE_INSTALLED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_state_install_projected",
            TargetStateInstallProjected,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_STATE_PROJECTED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_attachment_observed",
            TargetProcessAttachmentObserved,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_ATTACHMENT_OBSERVED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_attachment_projected",
            TargetProcessAttachmentProjected,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_ATTACHMENT_PROJECTED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_before_completion",
            TargetBeforeCompletion,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_ATTACHMENT_PROJECTED,
            RECOVERED_COMPLETED,
            ResumesExactOperation,
            Completed
        ),
        acceptance_case!(
            "target_completed_before_response",
            TargetCompletedBeforeResponse,
            Target,
            AmbiguousResponse,
            RestartTargetThenRetrySource,
            CUT_TARGET_COMPLETED,
            RECOVERED_COMPLETED,
            ObservesCompletedReceipt,
            Completed
        ),
    ];

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct HandoffMeasurementRecord {
        pub schema: String,
        pub case_id: String,
        pub workload_profile_id: String,
        pub source_site_id: String,
        pub target_site_id: String,
        pub object_schema_versions: BTreeMap<String, u32>,
        pub failure_cut: Option<HandoffCrashBoundary>,
        pub cache_state: String,
        pub object_count: u64,
        pub blob_count: u64,
        pub link_count: u64,
        pub total_bytes: u64,
        pub largest_entry_bytes: u64,
        pub target_present_entries: u64,
        pub target_present_bytes: u64,
        pub observed: HandoffObservedMeasurements,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct HandoffObservedMeasurements {
        pub closure_calculation_ms: u64,
        pub staging_and_transfer_ms: u64,
        pub closure_verification_ms: u64,
        pub source_publication_ms: u64,
        pub target_adoption_ms: u64,
        pub checkpoint_load_ms: u64,
        pub event_replay_ms: Option<u64>,
        pub project_materialization_ms: Option<u64>,
        pub worker_attach_recovery_ms: Option<u64>,
        pub total_handoff_recovery_ms: u64,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct HandoffMeasurementReport {
        pub schema: String,
        pub records: Vec<HandoffMeasurementRecord>,
    }

    impl HandoffMeasurementReport {
        pub const MAX_RECORDS: usize = 128;
        pub const MAX_ENCODED_BYTES: usize = 256 * 1024;

        pub fn validate(&self) -> Result<()> {
            if self.schema != "ryeos.worker_handoff_qualification_report.v1" {
                anyhow::bail!("unknown worker handoff qualification report schema");
            }
            if self.records.is_empty() || self.records.len() > Self::MAX_RECORDS {
                anyhow::bail!("worker handoff qualification report has an invalid record count");
            }
            for record in &self.records {
                if record.schema != "ryeos.worker_handoff_qualification_record.v1" {
                    anyhow::bail!("unknown worker handoff qualification record schema");
                }
                for (label, value) in [
                    ("case id", record.case_id.as_str()),
                    ("workload profile", record.workload_profile_id.as_str()),
                    ("source site", record.source_site_id.as_str()),
                    ("target site", record.target_site_id.as_str()),
                    ("cache state", record.cache_state.as_str()),
                ] {
                    if value.is_empty() || value.len() > 256 {
                        anyhow::bail!("handoff measurement {label} is empty or unbounded");
                    }
                }
                if record.object_schema_versions.is_empty()
                    || record.object_schema_versions.len() > 64
                    || record.total_bytes < record.largest_entry_bytes
                    || record.target_present_entries > record.object_count + record.blob_count
                    || record.target_present_bytes > record.total_bytes
                {
                    anyhow::bail!("handoff measurement closure summary is inconsistent");
                }
            }
            let encoded = lillux::canonical_json(&serde_json::to_value(self)?)?;
            if encoded.len() > Self::MAX_ENCODED_BYTES {
                anyhow::bail!("worker handoff qualification report exceeds its byte bound");
            }
            Ok(())
        }

        pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
            self.validate()?;
            Ok(lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes())
        }
    }

    /// A one-shot, test-owned gate. The selected boundary writes one bounded
    /// record to an inherited pipe and then parks forever so the parent can
    /// SIGKILL the daemon without running request or attempt destructors.
    pub struct HandoffPhaseGate {
        selected: HandoffCrashBoundary,
        writer: Mutex<std::fs::File>,
    }

    impl HandoffPhaseGate {
        pub fn new(selected: HandoffCrashBoundary, writer: std::fs::File) -> Self {
            Self {
                selected,
                writer: Mutex::new(writer),
            }
        }

        pub fn reach(&self, boundary: HandoffCrashBoundary) -> Result<()> {
            if boundary != self.selected {
                return Ok(());
            }
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| anyhow::anyhow!("handoff phase-gate writer lock poisoned"))?;
            writer
                .write_all(format!("{boundary}\n").as_bytes())
                .context("write handoff phase-cut evidence")?;
            writer.flush().context("flush handoff phase-cut evidence")?;
            drop(writer);
            loop {
                std::thread::park();
            }
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
    pub preflight_id: String,
    pub preflight_attestation_hash: String,
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
    pub follow_delivery_reservation_attestation_hash: Option<String>,
}

impl WorkerSessionHandoffJobOperation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        role: WorkerHandoffJobRole,
        operation_id: String,
        preflight_id: String,
        preflight_attestation_hash: String,
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
        follow_delivery_reservation_attestation_hash: Option<String>,
    ) -> anyhow::Result<Self> {
        let operation = Self {
            schema: HANDOFF_JOB_SCHEMA.to_owned(),
            operation_type: WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
            role,
            operation_id,
            preflight_id,
            preflight_attestation_hash,
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
            follow_delivery_reservation_attestation_hash,
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
        hash("handoff preflight", &self.preflight_id)?;
        hash(
            "handoff preflight attestation",
            &self.preflight_attestation_hash,
        )?;
        hash("handoff project route", &self.project_route_digest)?;
        if let Some(digest) = &self.follow_delivery_reservation_attestation_hash {
            hash("follow delivery reservation", digest)?;
        }
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
    pub abort_chain_head_hash: Option<String>,
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
            abort_chain_head_hash: None,
        };
        progress.validate()?;
        Ok(progress)
    }

    pub fn source_exported(operation_id: String) -> anyhow::Result<Self> {
        let mut progress = Self::planned(operation_id)?;
        progress.phase = WorkerHandoffPhase::SourceExported;
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
            ("abort chain head", self.abort_chain_head_hash.as_deref()),
        ] {
            if let Some(value) = value {
                hash(label, value)?;
            }
        }
        if let Some(reservation) = &self.credential_reservation_id {
            label_value("credential reservation", reservation)?;
        }
        if (self.phase == WorkerHandoffPhase::TargetPrepared
            || self.phase.successor_is_only_authorized_writer())
            && (self.placement_attestation_hash.is_none()
                || self.target_runtime_seed_hash.is_none()
                || self.credential_reservation_id.is_none())
        {
            bail!("target-prepared handoff progress is incomplete");
        }
        if self.phase.successor_is_only_authorized_writer()
            && (self.writer_grant_hash.is_none() || self.target_chain_head_hash.is_none())
        {
            bail!("source-committed handoff progress is incomplete");
        }
        if self.phase == WorkerHandoffPhase::AbortAuthorized && self.abort_chain_head_hash.is_none()
        {
            bail!("abort-authorized handoff progress has no source abort head");
        }
        if self.phase != WorkerHandoffPhase::AbortAuthorized && self.abort_chain_head_hash.is_some()
        {
            bail!("non-abort handoff progress carries abort authority");
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

impl WorkerPlacementPrepareRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (label, value) in [
            ("operation", self.operation_id.as_str()),
            ("preflight", self.preflight_id.as_str()),
            (
                "preflight attestation",
                self.preflight_attestation_hash.as_str(),
            ),
            ("source chain head", self.source_chain_head_hash.as_str()),
            ("transfer manifest", self.transfer_manifest_hash.as_str()),
            ("project route", self.project_route_digest.as_str()),
        ] {
            hash(label, value)?;
        }
        for (label, value) in [
            ("chain root", self.chain_root_id.as_str()),
            ("source site", self.source_site_id.as_str()),
            ("target site", self.target_site_id.as_str()),
            ("target project path", self.target_project_path.as_str()),
            (
                "target credential profile",
                self.target_credential_profile_id.as_str(),
            ),
        ] {
            label_value(label, value)?;
        }
        if self.source_site_id == self.target_site_id
            || !std::path::Path::new(&self.target_project_path).is_absolute()
        {
            bail!("placement preparation is not an exact cross-site project request");
        }
        if let Some(frontier) = &self.source_accounting_frontier {
            frontier.source_scope.validate()?;
        }
        if let Some(digest) = &self.follow_delivery_reservation_attestation_hash {
            hash("follow delivery reservation", digest)?;
        }
        Ok(())
    }
}

impl WorkerPlacementPreflightRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (label, value) in [
            ("preflight", self.preflight_id.as_str()),
            ("source chain head", self.source_chain_head_hash.as_str()),
            ("source last event", self.source_last_event_hash.as_str()),
            (
                "source launch capsule",
                self.source_launch_capsule_hash.as_str(),
            ),
            (
                "source launch metadata",
                self.source_launch_metadata_blob_hash.as_str(),
            ),
            ("project route", self.project_route_digest.as_str()),
            (
                "credential subject contract",
                self.credential_subject_contract_digest.as_str(),
            ),
            (
                "credential subject",
                self.credential_subject_digest.as_str(),
            ),
        ] {
            hash(label, value)?;
        }
        if let Some(digest) = &self.follow_delivery_reservation_attestation_hash {
            hash("follow delivery reservation", digest)?;
        }
        for (label, value) in [
            ("owner", self.owner_principal.as_str()),
            ("chain root", self.chain_root_id.as_str()),
            ("origin site", self.origin_site_id.as_str()),
            ("source site", self.source_site_id.as_str()),
            ("target site", self.target_site_id.as_str()),
            ("source placement", self.source_placement_thread_id.as_str()),
            (
                "successor placement",
                self.successor_placement_thread_id.as_str(),
            ),
            ("target project path", self.target_project_path.as_str()),
            (
                "target credential profile",
                self.target_credential_profile_id.as_str(),
            ),
            ("upstream session", self.upstream_session_id.as_str()),
        ] {
            label_value(label, value)?;
        }
        if self.source_site_id == self.target_site_id
            || !std::path::Path::new(&self.target_project_path).is_absolute()
        {
            bail!("worker placement preflight is not an exact cross-site project request");
        }
        let metadata_bytes = lillux::canonical_json(&self.source_launch_metadata)?;
        if metadata_bytes.len() > 2 * 1024 * 1024
            || lillux::sha256_hex(metadata_bytes.as_bytes())
                != self.source_launch_metadata_blob_hash
        {
            bail!("preflight launch metadata exceeds its bound or changed digest");
        }
        Ok(())
    }
}

impl WorkerPlacementPreflightEvidence {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request: &WorkerPlacementPreflightRequest,
        outer_exact_program_hash: String,
        persistent_dependency_programs: BTreeMap<String, String>,
        target_persistent_session_capsules: BTreeMap<String, String>,
        target_execution_realization_hash: String,
        target_isolation_digest: String,
        target_project_head_hash: String,
        target_credential_generation: u64,
    ) -> anyhow::Result<Self> {
        let evidence = Self {
            schema: PREFLIGHT_EVIDENCE_SCHEMA.to_owned(),
            operation_type: WORKER_SESSION_HANDOFF_PREFLIGHT_OPERATION.to_owned(),
            preflight_id: request.preflight_id.clone(),
            owner_principal: request.owner_principal.clone(),
            chain_root_id: request.chain_root_id.clone(),
            origin_site_id: request.origin_site_id.clone(),
            source_site_id: request.source_site_id.clone(),
            target_site_id: request.target_site_id.clone(),
            source_placement_thread_id: request.source_placement_thread_id.clone(),
            successor_placement_thread_id: request.successor_placement_thread_id.clone(),
            source_chain_head_hash: request.source_chain_head_hash.clone(),
            source_last_event_hash: request.source_last_event_hash.clone(),
            source_launch_capsule_hash: request.source_launch_capsule_hash.clone(),
            source_launch_metadata_blob_hash: request.source_launch_metadata_blob_hash.clone(),
            outer_exact_program_hash,
            persistent_dependency_programs,
            target_persistent_session_capsules,
            target_execution_realization_hash,
            target_isolation_digest,
            target_project_path: request.target_project_path.clone(),
            project_route_digest: request.project_route_digest.clone(),
            target_project_head_hash,
            target_credential_profile_id: request.target_credential_profile_id.clone(),
            target_credential_generation,
            upstream_session_id: request.upstream_session_id.clone(),
            credential_subject_contract_digest: request.credential_subject_contract_digest.clone(),
            credential_subject_digest: request.credential_subject_digest.clone(),
            follow_delivery_reservation_attestation_hash: request
                .follow_delivery_reservation_attestation_hash
                .clone(),
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PREFLIGHT_EVIDENCE_SCHEMA
            || self.operation_type != WORKER_SESSION_HANDOFF_PREFLIGHT_OPERATION
            || self.target_credential_generation == 0
        {
            bail!("worker placement preflight evidence is not the exact current contract");
        }
        for (label, value) in [
            ("preflight", self.preflight_id.as_str()),
            ("source chain head", self.source_chain_head_hash.as_str()),
            ("source last event", self.source_last_event_hash.as_str()),
            (
                "source launch capsule",
                self.source_launch_capsule_hash.as_str(),
            ),
            (
                "source launch metadata",
                self.source_launch_metadata_blob_hash.as_str(),
            ),
            (
                "outer exact program",
                self.outer_exact_program_hash.as_str(),
            ),
            (
                "target execution realization",
                self.target_execution_realization_hash.as_str(),
            ),
            ("target isolation", self.target_isolation_digest.as_str()),
            ("project route", self.project_route_digest.as_str()),
            (
                "target project head",
                self.target_project_head_hash.as_str(),
            ),
            (
                "credential subject contract",
                self.credential_subject_contract_digest.as_str(),
            ),
            (
                "credential subject",
                self.credential_subject_digest.as_str(),
            ),
        ] {
            hash(label, value)?;
        }
        if let Some(digest) = &self.follow_delivery_reservation_attestation_hash {
            hash("follow delivery reservation", digest)?;
        }
        for (label, value) in [
            ("owner", self.owner_principal.as_str()),
            ("chain root", self.chain_root_id.as_str()),
            ("origin site", self.origin_site_id.as_str()),
            ("source site", self.source_site_id.as_str()),
            ("target site", self.target_site_id.as_str()),
            ("source placement", self.source_placement_thread_id.as_str()),
            (
                "successor placement",
                self.successor_placement_thread_id.as_str(),
            ),
            ("target project path", self.target_project_path.as_str()),
            (
                "target credential profile",
                self.target_credential_profile_id.as_str(),
            ),
            ("upstream session", self.upstream_session_id.as_str()),
        ] {
            label_value(label, value)?;
        }
        if self.source_site_id == self.target_site_id
            || !std::path::Path::new(&self.target_project_path).is_absolute()
            || self.persistent_dependency_programs.is_empty()
            || self
                .persistent_dependency_programs
                .keys()
                .collect::<Vec<_>>()
                != self
                    .target_persistent_session_capsules
                    .keys()
                    .collect::<Vec<_>>()
        {
            bail!("worker placement preflight evidence has invalid placement coordinates");
        }
        for (label, entries) in [
            ("persistent program", &self.persistent_dependency_programs),
            (
                "target persistent-session capsule",
                &self.target_persistent_session_capsules,
            ),
        ] {
            for (name, digest) in entries {
                label_value("persistent dependency name", name)?;
                hash(label, digest)?;
            }
        }
        Ok(())
    }

    pub fn sign_attestation(&self, signer: &dyn Signer) -> anyhow::Result<Attestation> {
        self.validate()?;
        Attestation::unsigned(
            self.source_launch_capsule_hash.clone(),
            WORKER_PLACEMENT_PREFLIGHT_CLAIM.to_owned(),
            WORKER_PLACEMENT_PREFLIGHT_POLICY.to_owned(),
            lillux::time::iso8601_now(),
            None,
            serde_json::to_value(self)?,
        )
        .sign(signer)
    }

    pub fn from_attestation(attestation: &Attestation) -> anyhow::Result<Self> {
        if attestation.policy != WORKER_PLACEMENT_PREFLIGHT_POLICY
            || attestation.claim != WORKER_PLACEMENT_PREFLIGHT_CLAIM
        {
            bail!("attestation is not a worker placement preflight receipt");
        }
        let evidence: Self = serde_json::from_value(attestation.evidence.clone())?;
        evidence.validate()?;
        if attestation.subject_hash != evidence.source_launch_capsule_hash {
            bail!("preflight receipt subject differs from its source capsule");
        }
        Ok(evidence)
    }
}

impl WorkerPlacementPreflightResponse {
    pub fn validate_against(
        &self,
        request: &WorkerPlacementPreflightRequest,
        target_key: &lillux::crypto::VerifyingKey,
    ) -> anyhow::Result<()> {
        request.validate()?;
        self.evidence.validate()?;
        self.preflight_attestation.verify_with_key(target_key)?;
        let evidence =
            WorkerPlacementPreflightEvidence::from_attestation(&self.preflight_attestation)?;
        let attestation_hash =
            ryeos_state::objects::canonical_value_digest(&self.preflight_attestation.to_value())?;
        if self.preflight_id != request.preflight_id
            || self.preflight_attestation_hash != attestation_hash
            || self.evidence != evidence
            || evidence.preflight_id != request.preflight_id
            || evidence.owner_principal != request.owner_principal
            || evidence.chain_root_id != request.chain_root_id
            || evidence.origin_site_id != request.origin_site_id
            || evidence.source_site_id != request.source_site_id
            || evidence.target_site_id != request.target_site_id
            || evidence.source_placement_thread_id != request.source_placement_thread_id
            || evidence.successor_placement_thread_id != request.successor_placement_thread_id
            || evidence.source_chain_head_hash != request.source_chain_head_hash
            || evidence.source_last_event_hash != request.source_last_event_hash
            || evidence.source_launch_capsule_hash != request.source_launch_capsule_hash
            || evidence.source_launch_metadata_blob_hash != request.source_launch_metadata_blob_hash
            || evidence.target_project_path != request.target_project_path
            || evidence.project_route_digest != request.project_route_digest
            || evidence.target_credential_profile_id != request.target_credential_profile_id
            || evidence.upstream_session_id != request.upstream_session_id
            || evidence.credential_subject_contract_digest
                != request.credential_subject_contract_digest
            || evidence.credential_subject_digest != request.credential_subject_digest
            || evidence.follow_delivery_reservation_attestation_hash
                != request.follow_delivery_reservation_attestation_hash
        {
            bail!("worker placement preflight response contradicts its request");
        }
        Ok(())
    }
}

impl WorkerPlacementPreflightJobOperation {
    pub fn from_request(
        role: WorkerHandoffJobRole,
        peer_remote_name: String,
        request: &WorkerPlacementPreflightRequest,
    ) -> anyhow::Result<Self> {
        request.validate()?;
        let operation = Self {
            schema: PREFLIGHT_JOB_SCHEMA.to_owned(),
            operation_type: WORKER_SESSION_HANDOFF_PREFLIGHT_OPERATION.to_owned(),
            role,
            preflight_id: request.preflight_id.clone(),
            owner_principal: request.owner_principal.clone(),
            chain_root_id: request.chain_root_id.clone(),
            origin_site_id: request.origin_site_id.clone(),
            source_site_id: request.source_site_id.clone(),
            target_site_id: request.target_site_id.clone(),
            source_placement_thread_id: request.source_placement_thread_id.clone(),
            successor_placement_thread_id: request.successor_placement_thread_id.clone(),
            source_chain_head_hash: request.source_chain_head_hash.clone(),
            source_last_event_hash: request.source_last_event_hash.clone(),
            source_launch_capsule_hash: request.source_launch_capsule_hash.clone(),
            source_launch_metadata_blob_hash: request.source_launch_metadata_blob_hash.clone(),
            peer_remote_name,
            target_project_path: request.target_project_path.clone(),
            project_route_digest: request.project_route_digest.clone(),
            target_credential_profile_id: request.target_credential_profile_id.clone(),
            follow_delivery_reservation_attestation_hash: request
                .follow_delivery_reservation_attestation_hash
                .clone(),
        };
        operation.validate()?;
        Ok(operation)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PREFLIGHT_JOB_SCHEMA
            || self.operation_type != WORKER_SESSION_HANDOFF_PREFLIGHT_OPERATION
        {
            bail!("worker handoff preflight job is not the exact current contract");
        }
        for (label, value) in [
            ("preflight", self.preflight_id.as_str()),
            ("source chain head", self.source_chain_head_hash.as_str()),
            ("source last event", self.source_last_event_hash.as_str()),
            (
                "source launch capsule",
                self.source_launch_capsule_hash.as_str(),
            ),
            (
                "source launch metadata",
                self.source_launch_metadata_blob_hash.as_str(),
            ),
            ("project route", self.project_route_digest.as_str()),
        ] {
            hash(label, value)?;
        }
        for (label, value) in [
            ("owner", self.owner_principal.as_str()),
            ("chain root", self.chain_root_id.as_str()),
            ("origin site", self.origin_site_id.as_str()),
            ("source site", self.source_site_id.as_str()),
            ("target site", self.target_site_id.as_str()),
            ("source placement", self.source_placement_thread_id.as_str()),
            (
                "successor placement",
                self.successor_placement_thread_id.as_str(),
            ),
            ("peer remote", self.peer_remote_name.as_str()),
            ("target project path", self.target_project_path.as_str()),
            (
                "target credential profile",
                self.target_credential_profile_id.as_str(),
            ),
        ] {
            label_value(label, value)?;
        }
        if self.source_site_id == self.target_site_id
            || !std::path::Path::new(&self.target_project_path).is_absolute()
        {
            bail!("worker handoff preflight job has invalid cross-site coordinates");
        }
        if let Some(digest) = &self.follow_delivery_reservation_attestation_hash {
            hash("follow delivery reservation", digest)?;
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

impl WorkerPlacementAbortRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        hash("handoff abort operation", &self.operation_id)?;
        hash("handoff abort chain head", &self.abort_chain_head_hash)?;
        label_value("handoff abort chain root", &self.chain_root_id)
    }
}

impl WorkerPlacementAbortResponse {
    pub fn validate_against(&self, request: &WorkerPlacementAbortRequest) -> anyhow::Result<()> {
        if self.operation_id != request.operation_id
            || self.chain_root_id != request.chain_root_id
            || !matches!(
                self.disposition.as_str(),
                "reservation_released" | "already_released" | "target_absent"
            )
        {
            bail!("worker placement abort response changed its authority coordinates");
        }
        Ok(())
    }
}

/// Verify the source-signed, immediate abort successor that permanently makes
/// one pre-cut handoff operation ineligible for a writer transfer. The target
/// may release its reserved credential generation only after this evidence is
/// present in the exact source chain closure.
pub fn validate_handoff_abort_authority(
    cas: &lillux::CasStore,
    operation: &WorkerSessionHandoffJobOperation,
    abort_chain_head_hash: &str,
) -> anyhow::Result<()> {
    use ryeos_state::objects::{ChainState, ThreadEvent, ThreadStatus};

    operation.validate()?;
    hash("handoff abort chain head", abort_chain_head_hash)?;
    ryeos_state::sync::verify_chain_closure_anchored_pinned(
        cas,
        &operation.chain_root_id,
        abort_chain_head_hash,
        &operation.source_chain_head_hash,
    )?;
    let head_value = cas
        .get_object(abort_chain_head_hash)?
        .context("handoff abort chain head is absent")?;
    let head: ChainState = serde_json::from_value(head_value)?;
    head.validate()?;
    if head.chain_root_id != operation.chain_root_id
        || head.prev_chain_state_hash.as_deref() != Some(operation.source_chain_head_hash.as_str())
    {
        bail!("handoff abort is not the immediate source-head successor");
    }
    let source = head
        .threads
        .get(&operation.source_placement_thread_id)
        .context("handoff abort source placement is absent")?;
    if source.status == ThreadStatus::Continued {
        bail!("handoff abort cannot follow a committed continuation");
    }
    let event_hash = source
        .last_event_hash
        .as_deref()
        .context("handoff abort source has no terminal event")?;
    let event_value = cas
        .get_object(event_hash)?
        .context("handoff abort event is absent")?;
    let event: ThreadEvent = serde_json::from_value(event_value)?;
    event.validate()?;
    if event.event_type != "worker_session.handoff_aborted"
        || event.chain_root_id != operation.chain_root_id
        || event.thread_id != operation.source_placement_thread_id
        || event.prev_thread_event_hash.as_deref()
            != Some(operation.source_last_event_hash.as_str())
    {
        bail!("handoff abort event is not the immediate source-placement edge");
    }
    let expected = serde_json::json!({
        "schema":"ryeos.worker_session_handoff_abort.v1",
        "operation_id":operation.operation_id,
        "chain_root_id":operation.chain_root_id,
        "source_placement_thread_id":operation.source_placement_thread_id,
        "source_site_id":operation.source_site_id,
        "target_site_id":operation.target_site_id,
        "source_chain_head_hash":operation.source_chain_head_hash,
        "source_last_event_hash":operation.source_last_event_hash,
    });
    if ryeos_state::objects::canonical_value_digest(&event.payload)?
        != ryeos_state::objects::canonical_value_digest(&expected)?
    {
        bail!("handoff abort event differs from its durable operation");
    }
    Ok(())
}

impl WorkerPlacementPrepareResponse {
    pub fn validate_against(&self, request: &WorkerPlacementPrepareRequest) -> anyhow::Result<()> {
        if self.operation_id != request.operation_id {
            bail!("placement preparation response changed its operation id");
        }
        self.placement.validate()?;
        self.credential_reservation.validate()?;
        if self.placement.operation_id != request.operation_id
            || self.placement.preflight_id != request.preflight_id
            || self.placement.preflight_attestation_hash != request.preflight_attestation_hash
            || self.placement.follow_delivery_reservation_attestation_hash
                != request.follow_delivery_reservation_attestation_hash
            || self.placement.chain_root_id != request.chain_root_id
            || self.placement.source_site_id != request.source_site_id
            || self.placement.target_site_id != request.target_site_id
            || self.placement.source_chain_head_hash != request.source_chain_head_hash
            || self.placement.project_rebind.route_digest != request.project_route_digest
            || self.placement_attestation_hash.len() != 64
            || self.target_runtime_seed_hash != self.placement.target_runtime_seed_hash
            || self.target_launch_capsule_hash != self.placement.target_launch_capsule_hash
            || self.credential_reservation != self.placement.credential_reservation
        {
            bail!("placement preparation response contradicts its request or signed evidence");
        }
        hash("placement attestation", &self.placement_attestation_hash)?;
        Ok(())
    }
}

impl WorkerPlacementAdoptRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        label_value("adopt chain root", &self.chain_root_id)?;
        for (label, value) in [
            ("adopt operation", self.operation_id.as_str()),
            (
                "adopt target chain head",
                self.target_chain_head_hash.as_str(),
            ),
            (
                "adopt placement attestation",
                self.placement_attestation_hash.as_str(),
            ),
            ("adopt writer grant", self.writer_grant_hash.as_str()),
        ] {
            hash(label, value)?;
        }
        Ok(())
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
    pub preflight_id: String,
    pub preflight_attestation_hash: String,
    pub follow_delivery_reservation_attestation_hash: Option<String>,
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
        preflight_id: String,
        preflight_attestation_hash: String,
        follow_delivery_reservation_attestation_hash: Option<String>,
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
            preflight_id,
            preflight_attestation_hash,
            follow_delivery_reservation_attestation_hash,
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
        hash("placement preflight", &self.preflight_id)?;
        hash(
            "placement preflight attestation",
            &self.preflight_attestation_hash,
        )?;
        if let Some(digest) = &self.follow_delivery_reservation_attestation_hash {
            hash("follow delivery reservation", digest)?;
        }
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
    project_candidate_snapshot_hash: &str,
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
        project_candidate_snapshot_hash.to_owned(),
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
        match (&self.target_project_authority, &self.target_project_context) {
            (
                ExecutionProjectAuthority::PinnedGeneration {
                    display_path: Some(display_path),
                    realization: ryeos_state::objects::PinnedProjectRealization::Cow { .. },
                    ..
                },
                ryeos_engine::contracts::ProjectContext::LocalPath { path },
            ) if path == display_path => {}
            (ExecutionProjectAuthority::PinnedGeneration { .. }, _) => {
                bail!(
                    "remote worker adoption requires a writable pinned-COW authority resolved through its exact admitted target workspace path"
                )
            }
            _ => bail!("remote worker adoption requires a pinned-COW target project authority"),
        }
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

/// Derive the exact target-local pinned-COW authority from one admitted source
/// authority and a configured directional project endpoint. Paths and
/// environment-overlay identities are rebuilt for the target; immutable base,
/// capabilities, and child policy are copied exactly.
#[allow(clippy::too_many_arguments)]
pub fn build_remote_project_rebind(
    source: &ExecutionProjectAuthority,
    target_project_path: &std::path::Path,
    target_site_id: &str,
    owner_principal: &str,
    source_candidate_snapshot_hash: &str,
    target_expected_head_hash: &str,
    target_project_hash: &str,
    route_digest: &str,
) -> anyhow::Result<(
    ProjectAuthorityRebind,
    StableProjectIdentity,
    Option<PathBuf>,
)> {
    use ryeos_state::objects::{
        EnvironmentAuthority, PinnedProjectRealization, PinnedTerminalPublication,
    };
    source.validate()?;
    hash("source candidate snapshot", source_candidate_snapshot_hash)?;
    hash("target expected project head", target_expected_head_hash)?;
    hash("target project hash", target_project_hash)?;
    hash("project route", route_digest)?;
    let ExecutionProjectAuthority::PinnedGeneration {
        stable_project_identity: source_identity,
        base_snapshot_hash,
        realization: PinnedProjectRealization::Cow { .. },
        environment,
        capability_ceiling,
        child_policy,
        ..
    } = source
    else {
        bail!("remote worker handoff requires a pinned COW source project");
    };
    if target_expected_head_hash != base_snapshot_hash {
        bail!("target project HEAD is not the source base generation");
    }
    let target_identity = StableProjectIdentity::from_path(target_project_path, target_site_id)?;
    let (target_environment, target_overlay_root) = match environment {
        EnvironmentAuthority::ProjectOverlay {
            include_operator_vault,
            name_authority,
            ..
        } => (
            EnvironmentAuthority::ProjectOverlay {
                project_authority_id: lillux::sha256_hex(
                    format!(
                        "live-project\0{}\0{}",
                        target_identity.normalized_logical_key,
                        target_project_path.display()
                    )
                    .as_bytes(),
                ),
                source_identity: format!("dotenv:{}", target_project_path.join(".env").display()),
                include_operator_vault: *include_operator_vault,
                name_authority: name_authority.clone(),
            },
            Some(target_project_path.to_path_buf()),
        ),
        other => (other.clone(), None),
    };
    let target_authority = ExecutionProjectAuthority::PinnedGeneration {
        stable_project_identity: target_identity.normalized_logical_key.clone(),
        display_path: Some(target_project_path.to_path_buf()),
        base_snapshot_hash: base_snapshot_hash.clone(),
        snapshot_hash: source_candidate_snapshot_hash.to_owned(),
        realization: PinnedProjectRealization::Cow {
            terminal_publication: PinnedTerminalPublication::RetainCurrentHead {
                principal_key: ryeos_state::refs::principal_storage_key(owner_principal)?
                    .to_owned(),
                project_hash: target_project_hash.to_owned(),
                expected_hash: target_expected_head_hash.to_owned(),
            },
        },
        environment: target_environment,
        capability_ceiling: capability_ceiling.clone(),
        child_policy: child_policy.clone(),
    };
    let rebind = ProjectAuthorityRebind {
        route_digest: route_digest.to_owned(),
        source_stable_project_identity: source_identity.clone(),
        target_stable_project_identity: target_identity.normalized_logical_key.clone(),
        source_candidate_snapshot_hash: source_candidate_snapshot_hash.to_owned(),
        source_base_snapshot_hash: base_snapshot_hash.clone(),
        target_expected_head_hash: Some(target_expected_head_hash.to_owned()),
        target_authority,
    };
    rebind.validate_capsule_transition(source)?;
    Ok((rebind, target_identity, target_overlay_root))
}

pub fn build_target_accounting_conservation(
    source: Option<&crate::accounting_db::AccountingHandoffFrontier>,
    target_ledger_identity: Option<(String, u64)>,
    operation_id: &str,
) -> anyhow::Result<AccountingConservation> {
    hash("accounting handoff operation", operation_id)?;
    let Some(source) = source else {
        if target_ledger_identity.is_some() {
            bail!("accounting-free placement unexpectedly selected a target ledger");
        }
        return Ok(AccountingConservation {
            source_scope: None,
            target_scope: None,
            source_financial_high_water: 0,
            source_charged_usd_nanos: 0,
            source_remaining_cap_usd_nanos: None,
            target_cap_usd_nanos: None,
            source_remaining_directive_cap_usd_nanos: None,
            target_directive_cap_usd_nanos: None,
        });
    };
    let (target_budget_authority_site_id, epoch) = target_ledger_identity
        .filter(|(site_id, epoch)| !site_id.is_empty() && *epoch > 0)
        .ok_or_else(|| anyhow::anyhow!("accounted placement has no target accounting ledger"))?;
    let target_scope = AdmittedAccountingScope {
        budget_authority_site_id: target_budget_authority_site_id,
        ledger_epoch: epoch,
        execution_budget_id: format!("worker-handoff:{operation_id}"),
        directive_budget_id: source
            .source_scope
            .directive_budget_id
            .as_ref()
            .map(|_| format!("worker-handoff-directive:{operation_id}")),
    };
    target_scope.validate()?;
    let conservation = AccountingConservation {
        source_scope: Some(source.source_scope.clone()),
        target_scope: Some(target_scope),
        source_financial_high_water: source.financial_high_water,
        source_charged_usd_nanos: source.charged_usd_nanos,
        source_remaining_cap_usd_nanos: source.remaining_cap_usd_nanos,
        target_cap_usd_nanos: source.remaining_cap_usd_nanos,
        source_remaining_directive_cap_usd_nanos: source.remaining_directive_cap_usd_nanos,
        target_directive_cap_usd_nanos: source.remaining_directive_cap_usd_nanos,
    };
    conservation.validate()?;
    Ok(conservation)
}

/// Prove that final target admission retained the exact execution substrate
/// qualified by preflight. The full realization hash must change when the
/// final launch authority gains its settled accounting scope; every
/// substrate, contract, component, closure, and isolation property remains
/// byte-exact.
pub fn validate_target_realization_after_preflight(
    cas: &lillux::CasStore,
    preflight_hash: &str,
    final_hash: &str,
) -> anyhow::Result<()> {
    hash("preflight target realization", preflight_hash)?;
    hash("final target realization", final_hash)?;
    let load =
        |digest: &str| -> anyhow::Result<ryeos_state::objects::AdmittedExecutionRealization> {
            let value = cas
                .get_object(digest)?
                .with_context(|| format!("target execution realization {digest} is absent"))?;
            let realization =
                ryeos_state::objects::AdmittedExecutionRealization::from_current_value(&value)?;
            if realization.content_hash()? != digest {
                bail!("target execution realization changed content hash");
            }
            Ok(realization)
        };
    let mut preflight = load(preflight_hash)?;
    let final_realization = load(final_hash)?;
    preflight.launch_authority_digest = final_realization.launch_authority_digest.clone();
    if preflight != final_realization {
        bail!("final target execution substrate differs from preflight");
    }
    Ok(())
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

    struct PreflightTestSigner {
        signing_key: lillux::crypto::SigningKey,
        fingerprint: String,
    }

    impl PreflightTestSigner {
        fn new() -> Self {
            let signing_key = lillux::crypto::SigningKey::from_bytes(&[42; 32]);
            let fingerprint = lillux::crypto::fingerprint(&signing_key.verifying_key());
            Self {
                signing_key,
                fingerprint,
            }
        }
    }

    impl Signer for PreflightTestSigner {
        fn sign(&self, data: &[u8]) -> Vec<u8> {
            use lillux::crypto::Signer as _;
            self.signing_key.sign(data).to_bytes().to_vec()
        }

        fn fingerprint(&self) -> &str {
            &self.fingerprint
        }

        fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
            self.signing_key.verifying_key()
        }
    }

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
    fn handoff_accounting_uses_ledger_identity_not_placement_site_identity() {
        let frontier = crate::accounting_db::AccountingHandoffFrontier {
            source_scope: AdmittedAccountingScope {
                budget_authority_site_id: "S-source-ledger".into(),
                ledger_epoch: 7,
                execution_budget_id: "budget:source".into(),
                directive_budget_id: Some("directive:source".into()),
            },
            financial_high_water: 9,
            charged_usd_nanos: 3,
            remaining_cap_usd_nanos: Some(17),
            remaining_directive_cap_usd_nanos: Some(11),
        };
        let conservation = build_target_accounting_conservation(
            Some(&frontier),
            Some(("S-target-ledger".into(), 12)),
            &"a".repeat(64),
        )
        .unwrap();

        assert_eq!(
            conservation
                .source_scope
                .as_ref()
                .unwrap()
                .budget_authority_site_id,
            "S-source-ledger"
        );
        let target = conservation.target_scope.as_ref().unwrap();
        assert_eq!(target.budget_authority_site_id, "S-target-ledger");
        assert_eq!(target.ledger_epoch, 12);
        conservation.validate().unwrap();
    }

    #[test]
    fn final_target_realization_may_change_only_launch_authority_after_preflight() {
        let temp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(temp.path().join("cas"));
        let preflight = ryeos_state::objects::AdmittedExecutionRealization {
            schema: ryeos_state::objects::EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND.to_owned(),
            substrate_identity_hash: "1".repeat(64),
            substrate_attestation_hash: "2".repeat(64),
            launch_authority_digest: "3".repeat(64),
            effective_definition_digest: "4".repeat(64),
            artifact_identity_digest: "5".repeat(64),
            execution_closure_digest: "6".repeat(64),
            contract_ref: "runtime:test".to_owned(),
            contract_digest: "7".repeat(64),
            components: Vec::new(),
            properties: BTreeMap::new(),
        };
        let preflight_hash = cas.store_object(&preflight.to_value().unwrap()).unwrap();
        let mut final_realization = preflight.clone();
        final_realization.launch_authority_digest = "8".repeat(64);
        let final_hash = cas
            .store_object(&final_realization.to_value().unwrap())
            .unwrap();

        validate_target_realization_after_preflight(&cas, &preflight_hash, &final_hash).unwrap();

        final_realization.substrate_identity_hash = "9".repeat(64);
        let changed_substrate_hash = cas
            .store_object(&final_realization.to_value().unwrap())
            .unwrap();
        assert!(
            validate_target_realization_after_preflight(
                &cas,
                &preflight_hash,
                &changed_substrate_hash,
            )
            .is_err()
        );
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

    #[test]
    fn abort_progress_is_terminally_distinct_from_source_commit() {
        let mut progress = WorkerSessionHandoffProgress::planned("1".repeat(64)).unwrap();
        progress.phase = WorkerHandoffPhase::AbortAuthorized;
        assert!(progress.validate().is_err());
        progress.abort_chain_head_hash = Some("2".repeat(64));
        progress.validate().unwrap();

        progress.phase = WorkerHandoffPhase::SourceCommitted;
        progress.placement_attestation_hash = Some("3".repeat(64));
        progress.target_runtime_seed_hash = Some("4".repeat(64));
        progress.writer_grant_hash = Some("5".repeat(64));
        progress.target_chain_head_hash = Some("6".repeat(64));
        progress.credential_reservation_id = Some("reservation".into());
        assert!(
            progress
                .validate()
                .unwrap_err()
                .to_string()
                .contains("non-abort")
        );
    }

    #[test]
    fn source_exported_progress_is_an_exact_pre_cut_boundary() {
        let progress = WorkerSessionHandoffProgress::source_exported("1".repeat(64)).unwrap();
        assert_eq!(progress.phase, WorkerHandoffPhase::SourceExported);
        assert!(progress.placement_attestation_hash.is_none());
        assert!(progress.writer_grant_hash.is_none());
        progress.validate().unwrap();
    }

    #[test]
    fn every_handoff_phase_has_one_append_authority_lane() {
        for phase in WorkerHandoffPhase::ALL {
            assert_ne!(
                phase.source_is_only_authorized_writer(),
                phase.successor_is_only_authorized_writer(),
                "{} must belong to exactly one authority lane",
                phase.as_str()
            );
            if phase.target_has_adopted_writer_grant() {
                assert!(phase.successor_is_only_authorized_writer());
            }
        }
    }

    #[test]
    fn crash_boundary_inventory_covers_every_exact_seam_once() {
        use std::collections::BTreeSet;

        use test_support::{HANDOFF_CRASH_BOUNDARY_SPECS, HandoffCrashBoundary};

        let covered = HANDOFF_CRASH_BOUNDARY_SPECS
            .iter()
            .map(|spec| spec.boundary)
            .collect::<BTreeSet<_>>();
        assert_eq!(covered.len(), HANDOFF_CRASH_BOUNDARY_SPECS.len());
        assert_eq!(
            covered,
            HandoffCrashBoundary::ALL
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
        );
        serde_json::to_value(HANDOFF_CRASH_BOUNDARY_SPECS).unwrap();
    }

    #[test]
    fn qualification_matrix_covers_every_seam_with_unique_scenarios() {
        use std::collections::{BTreeMap, BTreeSet};

        use test_support::{
            HANDOFF_ACCEPTANCE_MATRIX, HANDOFF_CRASH_BOUNDARY_SPECS, HandoffCrashBoundary,
        };

        let specs = HANDOFF_CRASH_BOUNDARY_SPECS
            .iter()
            .map(|spec| (spec.boundary, spec.interrupted_node))
            .collect::<BTreeMap<_, _>>();
        let mut ids = BTreeSet::new();
        let mut covered = BTreeSet::new();
        for case in HANDOFF_ACCEPTANCE_MATRIX {
            assert!(ids.insert(case.case_id), "duplicate case {}", case.case_id);
            covered.insert(case.boundary);
            assert_eq!(
                specs.get(&case.boundary),
                Some(&case.interrupted_node),
                "{} interrupts the wrong node",
                case.case_id
            );
        }
        assert_eq!(
            covered,
            HandoffCrashBoundary::ALL
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
        );

        let encoded = serde_json::to_vec(HANDOFF_ACCEPTANCE_MATRIX).unwrap();
        assert!(encoded.len() < 128 * 1024, "acceptance oracle is unbounded");
    }

    #[test]
    fn qualification_matrix_never_derives_authority_from_progress_testimony() {
        use test_support::{
            AppendAuthorityLane, ExpectedHeadSigner, HANDOFF_ACCEPTANCE_MATRIX,
            SuccessorPlacementState,
        };

        let mut observed_unadopted_source_commit = false;
        for case in HANDOFF_ACCEPTANCE_MATRIX {
            for snapshot in [case.at_cut, case.after_recovery] {
                if snapshot.head_signer == ExpectedHeadSigner::TargetNode {
                    assert_eq!(
                        snapshot.append_authority,
                        AppendAuthorityLane::ExactSuccessorGrant,
                        "{}",
                        case.case_id
                    );
                }
                if snapshot.append_authority == AppendAuthorityLane::SourcePlacement {
                    assert_eq!(
                        snapshot.head_signer,
                        ExpectedHeadSigner::SourceNode,
                        "{}",
                        case.case_id
                    );
                }
                if snapshot.successor_placement == SuccessorPlacementState::PreparedOnly {
                    assert_eq!(
                        snapshot.append_authority,
                        AppendAuthorityLane::SourcePlacement,
                        "preparation must not grant append authority in {}",
                        case.case_id
                    );
                }
                if snapshot.target_phase == Some(WorkerHandoffPhase::SourceCommitted)
                    && snapshot.head_signer == ExpectedHeadSigner::SourceNode
                {
                    observed_unadopted_source_commit = true;
                }
            }
        }
        assert!(
            observed_unadopted_source_commit,
            "the oracle must prove target progress can testify to a source cut without becoming head authority"
        );
    }

    #[test]
    fn qualification_matrix_jobs_and_terminal_resources_are_exact() {
        use test_support::{
            CredentialDisposition, DurableJobExpectation, DurableJobPhase, DurableJobResult,
            DurableJobState, HANDOFF_ACCEPTANCE_MATRIX, StagingRootDisposition,
            WorkspaceDisposition,
        };

        fn validate_job(case_id: &str, job: DurableJobExpectation) {
            if job.state == DurableJobState::Absent {
                assert_eq!(job.phase, DurableJobPhase::Absent, "{case_id}");
                assert_eq!(job.result, DurableJobResult::Absent, "{case_id}");
                assert!(!job.active_attempt, "{case_id}");
            }
            if matches!(
                job.state,
                DurableJobState::Completed | DurableJobState::Cancelled
            ) {
                assert!(!job.active_attempt, "terminal attempt in {case_id}");
                assert!(
                    matches!(
                        job.result,
                        DurableJobResult::AdoptionReceipt | DurableJobResult::AbortReceipt
                    ),
                    "terminal job has no receipt in {case_id}"
                );
            }
        }

        for case in HANDOFF_ACCEPTANCE_MATRIX {
            for snapshot in [case.at_cut, case.after_recovery] {
                validate_job(case.case_id, snapshot.source_job);
                validate_job(case.case_id, snapshot.target_job);
            }
            let settled = case.after_recovery;
            assert_ne!(settled.target_credential, CredentialDisposition::Reserved);
            assert_ne!(
                settled.workspace,
                WorkspaceDisposition::EphemeralPreparation
            );
            assert_ne!(
                settled.source_staging_roots,
                StagingRootDisposition::ActiveJobOwned
            );
            assert_ne!(
                settled.target_staging_roots,
                StagingRootDisposition::ActiveJobOwned
            );
        }
    }

    #[test]
    fn abort_response_is_bound_to_one_operation_and_chain() {
        let request = WorkerPlacementAbortRequest {
            operation_id: "1".repeat(64),
            chain_root_id: "T-root".into(),
            abort_chain_head_hash: "2".repeat(64),
        };
        request.validate().unwrap();
        let mut response = WorkerPlacementAbortResponse {
            operation_id: request.operation_id.clone(),
            chain_root_id: request.chain_root_id.clone(),
            disposition: "reservation_released".into(),
        };
        response.validate_against(&request).unwrap();
        response.disposition = "released_without_chain_evidence".into();
        assert!(response.validate_against(&request).is_err());
    }

    fn preflight_request() -> WorkerPlacementPreflightRequest {
        let source_launch_metadata = serde_json::json!({"schema":1});
        WorkerPlacementPreflightRequest {
            preflight_id: "1".repeat(64),
            owner_principal: "fp:owner".into(),
            chain_root_id: "T-root".into(),
            origin_site_id: "site:a".into(),
            source_site_id: "site:a".into(),
            target_site_id: "site:b".into(),
            source_placement_thread_id: "T-source".into(),
            successor_placement_thread_id: "T-target".into(),
            source_chain_head_hash: "2".repeat(64),
            source_last_event_hash: "3".repeat(64),
            source_launch_capsule_hash: "4".repeat(64),
            source_launch_metadata_blob_hash: lillux::sha256_hex(
                lillux::canonical_json(&source_launch_metadata)
                    .unwrap()
                    .as_bytes(),
            ),
            source_launch_metadata,
            target_project_path: "/target/project".into(),
            project_route_digest: "5".repeat(64),
            target_credential_profile_id: "profile-target".into(),
            upstream_session_id: "upstream-session".into(),
            credential_subject_contract_digest: "6".repeat(64),
            credential_subject_digest: "7".repeat(64),
            follow_delivery_reservation_attestation_hash: None,
        }
    }

    fn preflight_evidence(
        request: &WorkerPlacementPreflightRequest,
    ) -> WorkerPlacementPreflightEvidence {
        WorkerPlacementPreflightEvidence::new(
            request,
            "8".repeat(64),
            BTreeMap::from([("worker".into(), "9".repeat(64))]),
            BTreeMap::from([("worker".into(), "a".repeat(64))]),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            3,
        )
        .unwrap()
    }

    #[test]
    fn preflight_receipt_is_target_key_and_request_bound() {
        let request = preflight_request();
        request.validate().unwrap();
        let evidence = preflight_evidence(&request);
        let target = PreflightTestSigner::new();
        let attestation = evidence.sign_attestation(&target).unwrap();
        let mut response = WorkerPlacementPreflightResponse {
            preflight_id: request.preflight_id.clone(),
            preflight_attestation_hash: ryeos_state::objects::canonical_value_digest(
                &attestation.to_value(),
            )
            .unwrap(),
            preflight_attestation: attestation,
            evidence,
        };
        response
            .validate_against(&request, &target.verifying_key())
            .unwrap();

        let mut wrong_owner = request.clone();
        wrong_owner.owner_principal = "fp:other-owner".into();
        assert!(
            response
                .validate_against(&wrong_owner, &target.verifying_key())
                .is_err()
        );

        response.evidence.target_credential_generation += 1;
        assert!(
            response
                .validate_against(&request, &target.verifying_key())
                .is_err()
        );
    }

    #[test]
    fn preflight_job_retains_only_non_authoritative_coordinates() {
        let request = preflight_request();
        let operation = WorkerPlacementPreflightJobOperation::from_request(
            WorkerHandoffJobRole::Target,
            "source-remote".into(),
            &request,
        )
        .unwrap();
        let decoded =
            WorkerPlacementPreflightJobOperation::from_value(operation.to_value().unwrap())
                .unwrap();
        assert_eq!(decoded, operation);
        assert_eq!(operation.owner_principal, request.owner_principal);
        assert!(!operation.to_value().unwrap().to_string().contains("token"));
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
            target_project_context: ProjectContext::LocalPath { path: path.clone() },
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

        let mut snapshot_only = remote_rebind();
        snapshot_only.target_project_context = ProjectContext::SnapshotHash {
            hash: "2".repeat(64),
        };
        assert!(
            source
                .for_remote_worker_adoption(&snapshot_only)
                .unwrap_err()
                .to_string()
                .contains("exact admitted target workspace path")
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

    #[test]
    fn qualification_measurement_report_is_bounded_and_canonical() {
        use crate::worker_handoff::test_support::{
            HandoffMeasurementRecord, HandoffMeasurementReport, HandoffObservedMeasurements,
        };

        let report = HandoffMeasurementReport {
            schema: "ryeos.worker_handoff_qualification_report.v1".into(),
            records: vec![HandoffMeasurementRecord {
                schema: "ryeos.worker_handoff_qualification_record.v1".into(),
                case_id: "portable_success".into(),
                workload_profile_id: "portable_framed_worker_v1".into(),
                source_site_id: "site:a".into(),
                target_site_id: "site:b".into(),
                object_schema_versions: BTreeMap::from([("worker_state_manifest".into(), 1)]),
                failure_cut: None,
                cache_state: "cold".into(),
                object_count: 3,
                blob_count: 2,
                link_count: 4,
                total_bytes: 4096,
                largest_entry_bytes: 2048,
                target_present_entries: 0,
                target_present_bytes: 0,
                observed: HandoffObservedMeasurements {
                    closure_calculation_ms: 1,
                    staging_and_transfer_ms: 2,
                    closure_verification_ms: 3,
                    source_publication_ms: 4,
                    target_adoption_ms: 5,
                    checkpoint_load_ms: 1,
                    event_replay_ms: None,
                    project_materialization_ms: None,
                    worker_attach_recovery_ms: None,
                    total_handoff_recovery_ms: 15,
                },
            }],
        };
        let first = report.canonical_bytes().unwrap();
        assert_eq!(first, report.canonical_bytes().unwrap());
        assert!(first.len() <= HandoffMeasurementReport::MAX_ENCODED_BYTES);

        let mut unbounded = report;
        unbounded.records =
            vec![unbounded.records[0].clone(); HandoffMeasurementReport::MAX_RECORDS + 1];
        assert!(unbounded.validate().is_err());
    }
}
