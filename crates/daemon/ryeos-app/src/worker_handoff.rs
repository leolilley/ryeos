//! Generic cross-site worker-placement authority.
//!
//! The wire object remains RyeOS's ordinary signed `Attestation`. These
//! structures make the evidence under the two handoff policies exhaustive and
//! typed so mutable sync-job phases or mere trust-store membership can never
//! be interpreted as placement or chain-writer authority.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use ryeos_state::objects::{AdmittedAccountingScope, Attestation, ExecutionProjectAuthority};
use ryeos_state::signer::Signer;

pub const WORKER_PLACEMENT_POLICY: &str = "worker-placement-v1";
pub const WORKER_PLACEMENT_CLAIM: &str = "admitted";
pub const WORKER_SESSION_HANDOFF_OPERATION: &str = "worker_session_handoff";

const PLACEMENT_EVIDENCE_SCHEMA: &str = "ryeos.worker_placement_admission.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialGenerationReservation {
    pub profile_id: String,
    pub owner_principal: String,
    pub generation: u64,
    pub reservation_id: String,
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

impl CredentialGenerationReservation {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (label, value) in [
            ("credential profile", self.profile_id.as_str()),
            ("credential owner", self.owner_principal.as_str()),
            ("credential reservation", self.reservation_id.as_str()),
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
        match (
            self.source_remaining_cap_usd_nanos,
            self.target_cap_usd_nanos,
        ) {
            (Some(source), Some(target)) if target <= source => {}
            (None, None) => {}
            _ => bail!("target accounting cap must be present and no larger than source remainder"),
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

    fn accounting() -> AccountingConservation {
        AccountingConservation {
            source_scope: None,
            target_scope: None,
            source_financial_high_water: 4,
            source_charged_usd_nanos: 5,
            source_remaining_cap_usd_nanos: None,
            target_cap_usd_nanos: None,
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
            subject_contract_digest: "1".repeat(64),
            subject_digest: "2".repeat(64),
        };
        assert!(reservation.validate().is_ok());
        reservation.generation = 0;
        assert!(reservation.validate().is_err());
    }
}
