//! Target-signed testimony for returning one retained hosted-session project
//! candidate to its configured owner.
//!
//! The testimony is evidence, not publication authority. The source still
//! imports the exact candidate closure and applies it through the ordinary
//! clean-base project-result path; the target candidate remains retained until
//! a separate owner-authorized publish or discard operation. Frozen/retained
//! describes the candidate, not the worker root's lifecycle or qualification.

use anyhow::{Context as _, Result, bail};
use ryeos_state::objects::Attestation;
use ryeos_state::signer::Signer;
use serde::{Deserialize, Serialize};

use crate::dedicated_session_service::HostedCommandCompletionFence;

pub const HOSTED_CANDIDATE_RESULT_CLAIM: &str = "hosted_terminal_candidate_result";
pub const HOSTED_CANDIDATE_RESULT_POLICY: &str = "ryeos.hosted_terminal_candidate_result.v2";
pub const HOSTED_CANDIDATE_RESULT_SCHEMA: &str =
    "ryeos.hosted_terminal_candidate_result_evidence.v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedCandidateState {
    Frozen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedCandidateDisposition {
    Retained,
}

/// Fresh closure/base verification, not evaluator qualification and not a
/// synthetic validation event appended to an already completed worker root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedCandidateClosureValidation {
    pub schema: String,
    pub checks: HostedCandidateClosureChecks,
    pub candidate_snapshot_hash: String,
    pub candidate_validation_hash: String,
    pub base_snapshot_hash: String,
    pub candidate_tree_hash: String,
    pub base_tree_hash: String,
    pub candidate_policy_hash: String,
    pub changed_path_count: u64,
    pub changed_paths_digest: String,
    pub object_count: u64,
    pub blob_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedCandidateClosureChecks {
    pub canonical_snapshot_closure: bool,
    pub base_ancestry: bool,
}

impl HostedCandidateClosureValidation {
    pub fn content_hash(&self) -> Result<String> {
        ryeos_state::objects::canonical_value_digest(&serde_json::to_value(self)?)
    }

    fn validate_for(&self, base: &str, candidate: &str, validation: &str) -> Result<()> {
        if self.schema != "ryeos.hosted_candidate_closure_and_base_validation.v2"
            || !self.checks.canonical_snapshot_closure
            || !self.checks.base_ancestry
            || self.base_snapshot_hash != base
            || self.candidate_snapshot_hash != candidate
            || self.candidate_validation_hash != validation
            || crate::thread_lifecycle::candidate_validation_identity(candidate)? != validation
            || self.object_count == 0
        {
            bail!("hosted candidate closure/base evidence contradicts its exact candidate");
        }
        for hash in [
            &self.candidate_tree_hash,
            &self.base_tree_hash,
            &self.candidate_policy_hash,
            &self.changed_paths_digest,
        ] {
            if !canonical_hash(hash) {
                bail!("hosted candidate closure/base evidence contains a noncanonical hash");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedCandidateResultRequest {
    pub chain_root_id: String,
    pub source_site_id: String,
}

impl HostedCandidateResultRequest {
    pub fn validate(&self) -> Result<()> {
        ryeos_runtime::validate_runtime_thread_id(&self.chain_root_id)
            .map_err(|error| anyhow::anyhow!(error))
            .context("hosted candidate result chain root is not canonical")?;
        crate::identity::validate_canonical_site_id(&self.source_site_id)
            .context("hosted candidate result source site is not canonical")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedCandidateResultEvidence {
    pub schema: String,
    pub owner_principal: String,
    pub source_site_id: String,
    pub target_site_id: String,
    pub chain_root_id: String,
    pub placement_thread_id: String,
    pub chain_head_hash: String,
    pub last_event_hash: String,
    pub admitted_launch_capsule_hash: String,
    pub admitted_session_capsule_hash: String,
    pub stable_project_identity: String,
    pub target_project_path: String,
    pub base_snapshot_hash: String,
    pub candidate_snapshot_hash: String,
    pub candidate_validation_hash: String,
    pub completion_fence: HostedCommandCompletionFence,
    pub command_response_digest: String,
    pub candidate_capture_operation_id: String,
    pub closure_validation: HostedCandidateClosureValidation,
    pub closure_validation_hash: String,
    pub candidate_state: HostedCandidateState,
    pub disposition: HostedCandidateDisposition,
}

impl HostedCandidateResultEvidence {
    pub fn validate(&self) -> Result<()> {
        if self.schema != HOSTED_CANDIDATE_RESULT_SCHEMA {
            bail!("hosted candidate result evidence schema is not current");
        }
        let owner = self
            .owner_principal
            .strip_prefix("fp:")
            .context("hosted candidate result owner is not canonical")?;
        if !canonical_hash(owner) {
            bail!("hosted candidate result owner is not canonical");
        }
        crate::identity::validate_canonical_site_id(&self.source_site_id)?;
        crate::identity::validate_canonical_site_id(&self.target_site_id)?;
        ryeos_runtime::validate_runtime_thread_id(&self.chain_root_id)
            .map_err(|error| anyhow::anyhow!(error))?;
        ryeos_runtime::validate_runtime_thread_id(&self.placement_thread_id)
            .map_err(|error| anyhow::anyhow!(error))?;
        for (label, hash) in [
            ("chain head", self.chain_head_hash.as_str()),
            ("last event", self.last_event_hash.as_str()),
            (
                "admitted launch capsule",
                self.admitted_launch_capsule_hash.as_str(),
            ),
            (
                "admitted session capsule",
                self.admitted_session_capsule_hash.as_str(),
            ),
            ("base snapshot", self.base_snapshot_hash.as_str()),
            ("candidate snapshot", self.candidate_snapshot_hash.as_str()),
            (
                "candidate validation",
                self.candidate_validation_hash.as_str(),
            ),
            (
                "candidate capture operation",
                self.candidate_capture_operation_id.as_str(),
            ),
            ("closure validation", self.closure_validation_hash.as_str()),
            ("command response", self.command_response_digest.as_str()),
        ] {
            if !canonical_hash(hash) {
                bail!("hosted candidate result {label} hash is not canonical");
            }
        }
        self.closure_validation.validate_for(
            &self.base_snapshot_hash,
            &self.candidate_snapshot_hash,
            &self.candidate_validation_hash,
        )?;
        if self.closure_validation_hash != self.closure_validation.content_hash()? {
            bail!("hosted candidate closure/base evidence digest changed");
        }
        if self.stable_project_identity.trim().is_empty()
            || self.stable_project_identity.chars().any(char::is_control)
        {
            bail!("hosted candidate result stable project identity is invalid");
        }
        let expected_project_identity = crate::launch_metadata::StableProjectIdentity::from_path(
            std::path::Path::new(&self.target_project_path),
            &self.target_site_id,
        )
        .context("hosted candidate result target project path is invalid")?;
        if self.stable_project_identity != expected_project_identity.normalized_logical_key {
            bail!("hosted candidate result stable project identity changed");
        }
        if self.completion_fence.placement_thread_id != self.placement_thread_id
            || self.completion_fence.admitted_capsule_hash != self.admitted_session_capsule_hash
            || self.completion_fence.worker_boot_epoch == 0
            || self.completion_fence.command_sequence == 0
            || !canonical_hash(&self.completion_fence.request_digest)
            || !canonical_hash(&self.completion_fence.completion_operation_id)
            || self.completion_fence.turn_id.is_empty()
            || self.completion_fence.turn_id.len() > 256
            || self.completion_fence.turn_id.chars().any(char::is_control)
        {
            bail!("hosted candidate result completion fence changed placement authority");
        }
        Ok(())
    }

    pub fn content_hash(&self) -> Result<String> {
        self.validate()?;
        ryeos_state::objects::canonical_value_digest(&serde_json::to_value(self)?)
    }

    pub fn sign_attestation(&self, signer: &dyn Signer) -> Result<Attestation> {
        self.validate()?;
        Attestation::unsigned(
            self.candidate_snapshot_hash.clone(),
            HOSTED_CANDIDATE_RESULT_CLAIM.to_owned(),
            HOSTED_CANDIDATE_RESULT_POLICY.to_owned(),
            lillux::time::iso8601_now(),
            None,
            serde_json::to_value(self)?,
        )
        .sign(signer)
    }

    pub fn from_attestation(attestation: &Attestation) -> Result<Self> {
        if attestation.claim != HOSTED_CANDIDATE_RESULT_CLAIM
            || attestation.policy != HOSTED_CANDIDATE_RESULT_POLICY
            || attestation
                .evidence
                .get("schema")
                .and_then(serde_json::Value::as_str)
                != Some(HOSTED_CANDIDATE_RESULT_SCHEMA)
        {
            bail!("attestation is not hosted candidate-result testimony");
        }
        let evidence: Self = serde_json::from_value(attestation.evidence.clone())?;
        evidence.validate()?;
        if attestation.subject_hash != evidence.candidate_snapshot_hash {
            bail!("candidate-result testimony subject differs from its candidate");
        }
        Ok(evidence)
    }
}

fn canonical_hash(value: &str) -> bool {
    lillux::valid_hash(value) && !value.bytes().any(|byte| byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestSigner {
        signing_key: lillux::crypto::SigningKey,
        fingerprint: String,
    }

    impl TestSigner {
        fn new() -> Self {
            let signing_key = lillux::crypto::SigningKey::from_bytes(&[23_u8; 32]);
            let fingerprint = lillux::crypto::fingerprint(&signing_key.verifying_key());
            Self {
                signing_key,
                fingerprint,
            }
        }
    }

    impl Signer for TestSigner {
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

    fn evidence() -> HostedCandidateResultEvidence {
        let target_project_path = "/srv/projects/example".to_owned();
        let stable_project_identity = crate::launch_metadata::StableProjectIdentity::from_path(
            std::path::Path::new(&target_project_path),
            "site:target",
        )
        .unwrap()
        .normalized_logical_key;
        let validation =
            crate::thread_lifecycle::candidate_validation_identity(&"6".repeat(64)).unwrap();
        let closure_validation = HostedCandidateClosureValidation {
            schema: "ryeos.hosted_candidate_closure_and_base_validation.v2".to_owned(),
            checks: HostedCandidateClosureChecks {
                canonical_snapshot_closure: true,
                base_ancestry: true,
            },
            candidate_snapshot_hash: "6".repeat(64),
            candidate_validation_hash: validation.clone(),
            base_snapshot_hash: "5".repeat(64),
            candidate_tree_hash: "d".repeat(64),
            base_tree_hash: "e".repeat(64),
            candidate_policy_hash: "f".repeat(64),
            changed_path_count: 1,
            changed_paths_digest: "0".repeat(64),
            object_count: 4,
            blob_count: 1,
        };
        HostedCandidateResultEvidence {
            schema: HOSTED_CANDIDATE_RESULT_SCHEMA.to_owned(),
            owner_principal: format!("fp:{}", "1".repeat(64)),
            source_site_id: "site:source".to_owned(),
            target_site_id: "site:target".to_owned(),
            chain_root_id: "T-root".to_owned(),
            placement_thread_id: "T-placement".to_owned(),
            chain_head_hash: "2".repeat(64),
            last_event_hash: "3".repeat(64),
            admitted_launch_capsule_hash: "b".repeat(64),
            admitted_session_capsule_hash: "4".repeat(64),
            stable_project_identity,
            target_project_path,
            base_snapshot_hash: "5".repeat(64),
            candidate_snapshot_hash: "6".repeat(64),
            candidate_validation_hash: validation,
            completion_fence: HostedCommandCompletionFence {
                placement_thread_id: "T-placement".to_owned(),
                admitted_capsule_hash: "4".repeat(64),
                worker_boot_epoch: 2,
                command_sequence: 3,
                request_digest: "8".repeat(64),
                turn_id: "turn-1".to_owned(),
                completion_operation_id: "9".repeat(64),
            },
            command_response_digest: "c".repeat(64),
            candidate_capture_operation_id: "a".repeat(64),
            closure_validation_hash: closure_validation.content_hash().unwrap(),
            closure_validation,
            candidate_state: HostedCandidateState::Frozen,
            disposition: HostedCandidateDisposition::Retained,
        }
    }

    #[test]
    fn signed_result_is_bound_to_request_owner_and_target() {
        let signer = TestSigner::new();
        let request = HostedCandidateResultRequest {
            chain_root_id: "T-root".to_owned(),
            source_site_id: "site:source".to_owned(),
        };
        let response = HostedCandidateResultResponse::from_value(
            serde_json::to_value(HostedCandidateResultResponse::new(evidence(), &signer).unwrap())
                .unwrap(),
        )
        .unwrap();
        response
            .validate_against(
                &request,
                &format!("fp:{}", "1".repeat(64)),
                "site:target",
                &signer.verifying_key(),
            )
            .unwrap();

        let mut changed = response.clone();
        changed.evidence.base_snapshot_hash = "c".repeat(64);
        assert!(
            changed
                .validate_against(
                    &request,
                    &format!("fp:{}", "1".repeat(64)),
                    "site:target",
                    &signer.verifying_key(),
                )
                .is_err()
        );
    }

    #[test]
    fn frozen_result_binds_closure_evidence_without_claiming_qualification() {
        let evidence = evidence();
        evidence.validate().unwrap();
        let value = serde_json::to_value(&evidence).unwrap();
        assert_eq!(value["candidate_state"], "frozen");
        assert_eq!(value["disposition"], "retained");
        assert!(value.get("candidate_validation_operation_id").is_none());
        let mut changed = evidence.clone();
        changed.closure_validation.object_count += 1;
        assert!(changed.validate().is_err());
        changed.closure_validation.base_snapshot_hash = "a".repeat(64);
        changed.closure_validation_hash = changed.closure_validation.content_hash().unwrap();
        assert!(changed.validate().is_err());
    }

    #[test]
    fn candidate_result_rejects_old_contract_and_validation_fact_alias() {
        let error = HostedCandidateResultResponse::from_value(serde_json::json!({
            "evidence":{"schema":"ryeos.hosted_terminal_candidate_result_evidence.v1"},
            "attestation":"not current testimony",
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("evidence schema is not current"));
        let mut value = serde_json::to_value(evidence()).unwrap();
        value["candidate_state"] = "publish_ready".into();
        assert!(serde_json::from_value::<HostedCandidateResultEvidence>(value).is_err());
        let mut value = serde_json::to_value(evidence()).unwrap();
        value["candidate_validation_operation_id"] = "b".repeat(64).into();
        assert!(serde_json::from_value::<HostedCandidateResultEvidence>(value).is_err());
        let mut old = evidence();
        old.schema = "ryeos.hosted_terminal_candidate_result_evidence.v1".to_owned();
        assert!(old.validate().is_err());
        let signer = TestSigner::new();
        let mut attestation = evidence().sign_attestation(&signer).unwrap();
        attestation.policy = "ryeos.hosted_terminal_candidate_result.v1".to_owned();
        assert!(HostedCandidateResultEvidence::from_attestation(&attestation).is_err());
    }

    #[test]
    fn candidate_result_keeps_launch_and_session_capsule_coordinates_distinct() {
        let evidence = evidence();
        assert_ne!(
            evidence.admitted_launch_capsule_hash,
            evidence.admitted_session_capsule_hash
        );
        evidence.validate().unwrap();
        let mut changed = evidence.clone();
        changed.completion_fence.admitted_capsule_hash = evidence.admitted_launch_capsule_hash;
        assert!(changed.validate().is_err());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedCandidateResultResponse {
    pub evidence_hash: String,
    pub attestation_hash: String,
    pub attestation: Attestation,
    pub evidence: HostedCandidateResultEvidence,
}

impl HostedCandidateResultResponse {
    pub fn from_value(value: serde_json::Value) -> Result<Self> {
        // Classify the current contract before decoding its nested evidence.
        // No predecessor can be interpreted as the frozen-candidate shape.
        if value
            .pointer("/evidence/schema")
            .and_then(serde_json::Value::as_str)
            != Some(HOSTED_CANDIDATE_RESULT_SCHEMA)
        {
            bail!("hosted candidate result evidence schema is not current");
        }
        let response: Self = serde_json::from_value(value)?;
        response.evidence.validate()?;
        Ok(response)
    }

    pub fn new(evidence: HostedCandidateResultEvidence, signer: &dyn Signer) -> Result<Self> {
        let evidence_hash = evidence.content_hash()?;
        let attestation = evidence.sign_attestation(signer)?;
        let attestation_hash =
            ryeos_state::objects::canonical_value_digest(&attestation.to_value())?;
        Ok(Self {
            evidence_hash,
            attestation_hash,
            attestation,
            evidence,
        })
    }

    pub fn validate_against(
        &self,
        request: &HostedCandidateResultRequest,
        expected_owner_principal: &str,
        expected_target_site_id: &str,
        target_key: &lillux::crypto::VerifyingKey,
    ) -> Result<()> {
        request.validate()?;
        self.evidence.validate()?;
        self.attestation.verify_with_key(target_key)?;
        let attested = HostedCandidateResultEvidence::from_attestation(&self.attestation)?;
        if self.evidence != attested
            || self.evidence_hash != self.evidence.content_hash()?
            || self.attestation_hash
                != ryeos_state::objects::canonical_value_digest(&self.attestation.to_value())?
            || self.evidence.owner_principal != expected_owner_principal
            || self.evidence.source_site_id != request.source_site_id
            || self.evidence.target_site_id != expected_target_site_id
            || self.evidence.chain_root_id != request.chain_root_id
        {
            bail!("hosted candidate-result response changed its requested authority");
        }
        Ok(())
    }
}
