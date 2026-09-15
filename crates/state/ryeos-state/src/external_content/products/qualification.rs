//! Finite product qualification policy and compact node testimony.
//!
//! These types do not execute a verifier or authorize its claims. The app
//! proves an independently completed admitted verifier, including its actual
//! subject realization, before signing. Attestation's existing subject edge
//! retains the product manifest, while the typed qualification contract retains
//! its exact product/qualification witnesses. Historical program and terminal
//! coordinates remain non-owning node testimony, not independent full-run proof.

use std::collections::BTreeSet;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::composition::ResolvedExternalProductSelections;
use super::publication::ProductCaptureCoordinate;
use super::{validate_canonical_unsuffixed_ref, validate_hash, validate_name};
use crate::Signer;
use crate::objects::{AdmittedLaunchArtifactIdentity, Attestation, canonical_value_digest};

pub const PRODUCT_QUALIFICATION_POLICY_SCHEMA: &str = "ryeos.product_qualification_policy.v1";
pub const PRODUCT_QUALIFICATION_RESULT_SCHEMA: &str = "ryeos.product_qualification_result.v1";
pub const PRODUCT_QUALIFICATION_EVIDENCE_SCHEMA: &str = "ryeos.product_qualification_evidence.v5";
pub const PRODUCT_QUALIFICATION_ATTESTATION_POLICY: &str = "ryeos.product_qualification.v1";
pub const PRODUCT_QUALIFICATION_CLAIM: &str = "retained_product_qualified";
pub const MAX_PRODUCT_QUALIFICATION_CLAIMS: usize = 32;
pub const MAX_PRODUCT_QUALIFICATION_POLICY_BYTES: usize = 16 * 1024;
pub const MAX_PRODUCT_QUALIFICATION_RESULT_BYTES: usize = 16 * 1024;
pub const MAX_PRODUCT_QUALIFICATION_SELECTIONS_BYTES: usize = 16 * 1024;
pub const MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES: usize = 64 * 1024;
pub const MAX_PRODUCT_QUALIFICATION_PARTICIPANTS: usize = 16;
pub const MAX_PRODUCT_QUALIFICATION_CALL_ID_BYTES: usize = 128;
const MAX_PROBE_VALUE_BYTES: usize = 8 * 1024;

/// One signed Config's finite verifier allowance. Probe parameters are static
/// signed inputs; output target compatibility is ecosystem meaning, not a
/// daemon interpretation of the builder's platform or an executable filename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationPolicy {
    pub schema: String,
    pub verifier_ref: String,
    pub subject_declaration_id: String,
    pub allowed_claims: Vec<String>,
    pub verifier_parameters: Value,
}

impl ProductQualificationPolicy {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        bounded(
            value,
            MAX_PRODUCT_QUALIFICATION_POLICY_BYTES,
            "qualification policy",
        )?;
        let policy: Self =
            serde_json::from_value(value.clone()).context("decode qualification policy")?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PRODUCT_QUALIFICATION_POLICY_SCHEMA {
            bail!("unsupported product qualification policy schema");
        }
        validate_canonical_unsuffixed_ref("qualification verifier", &self.verifier_ref)?;
        validate_name(&self.subject_declaration_id)?;
        validate_claims(&self.allowed_claims)?;
        bounded_object(
            &self.verifier_parameters,
            "qualification verifier parameters",
        )?;
        bounded(
            self,
            MAX_PRODUCT_QUALIFICATION_POLICY_BYTES,
            "qualification policy",
        )
    }

    pub fn admitted_parameters_digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        // Same canonical projection as SealedRootExecutionRequest; no second
        // parameter identity and no dynamic caller additions to signed inputs.
        canonical_value_digest(&self.verifier_parameters)
    }
}

/// Retained identity of the signed policy selected by the authorized owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationPolicySource {
    pub canonical_ref: String,
    pub raw_content_digest: String,
    pub effective_definition_digest: String,
    pub publisher_fingerprint: String,
    pub policy: ProductQualificationPolicy,
}

impl ProductQualificationPolicySource {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_canonical_unsuffixed_ref("qualification policy", &self.canonical_ref)?;
        if !self.canonical_ref.starts_with("config:") {
            bail!("product qualification policy must be a Config");
        }
        validate_hash("qualification policy source", &self.raw_content_digest)?;
        validate_hash(
            "qualification policy definition",
            &self.effective_definition_digest,
        )?;
        validate_hash(
            "qualification policy publisher",
            &self.publisher_fingerprint,
        )?;
        self.policy.validate()
    }
}

/// Affirmative finite claims over the exact post-capture subject. Merely
/// exiting successfully does not produce this result. Probe evidence remains
/// bounded opaque ecosystem data and cannot choose policy or verifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationResult {
    pub schema: String,
    pub subject_manifest_hash: String,
    pub claims: Vec<String>,
    pub probe_evidence: Value,
}

impl ProductQualificationResult {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        bounded(
            value,
            MAX_PRODUCT_QUALIFICATION_RESULT_BYTES,
            "qualification result",
        )?;
        let result: Self =
            serde_json::from_value(value.clone()).context("decode qualification result")?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PRODUCT_QUALIFICATION_RESULT_SCHEMA {
            bail!("unsupported product qualification result schema");
        }
        validate_hash("qualified product subject", &self.subject_manifest_hash)?;
        validate_claims(&self.claims)?;
        bounded_object(&self.probe_evidence, "qualification probe evidence")?;
        bounded(
            self,
            MAX_PRODUCT_QUALIFICATION_RESULT_BYTES,
            "qualification result",
        )
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::to_value(self)?)
    }

    pub fn validate_claims_for(
        &self,
        policy: &ProductQualificationPolicy,
        required: &[String],
    ) -> anyhow::Result<()> {
        self.validate()?;
        policy.validate()?;
        validate_claims_allow_empty(required)?;
        if self
            .claims
            .iter()
            .any(|claim| policy.allowed_claims.binary_search(claim).is_err())
            || required
                .iter()
                .any(|claim| self.claims.binary_search(claim).is_err())
        {
            bail!("qualification claims exceed the signed allowance or omit a required claim");
        }
        Ok(())
    }
}

/// Daemon-projected facts from an actual admitted verifier and its exact
/// successful terminal. The substrate identity comes from its admitted
/// execution realization, never host discovery or caller environment metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationVerifier {
    pub chain_root_id: String,
    pub thread_id: String,
    pub admitted_launch_capsule_hash: String,
    pub canonical_ref: String,
    pub effective_definition_digest: String,
    pub exact_program_hash: String,
    pub admitted_parameters_digest: String,
    pub launch_authority_digest: String,
    pub execution_realization_hash: String,
    /// Executable, protocol and runtime identity, not just the item's D2.
    pub artifact_identity: AdmittedLaunchArtifactIdentity,
    /// Existing normalized direct-plan execution context, not a source root
    /// or permission to read a historical project. Managed verifiers use null.
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub admitted_project_root: Option<std::path::PathBuf>,
    pub substrate_identity_hash: String,
    pub subject_declaration_id: String,
    pub subject_manifest_hash: String,
    pub terminal_snapshot_hash: String,
    pub result_digest: String,
}

impl ProductQualificationVerifier {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.artifact_identity.validate()?;
        if let Some(root) = &self.admitted_project_root {
            if root.as_os_str()
                != std::ffi::OsStr::new(crate::objects::ADMITTED_DIRECT_PROJECT_ROOT)
                || !matches!(
                    &self.artifact_identity,
                    AdmittedLaunchArtifactIdentity::DirectItemExecutor { .. }
                )
            {
                bail!("qualification verifier has no canonical direct execution root context");
            }
        }
        for (label, value) in [
            ("verifier chain root", &self.chain_root_id),
            ("verifier thread", &self.thread_id),
        ] {
            exact_coordinate(label, value)?;
        }
        validate_canonical_unsuffixed_ref("qualification verifier", &self.canonical_ref)?;
        validate_name(&self.subject_declaration_id)?;
        for (label, hash) in [
            ("verifier capsule", &self.admitted_launch_capsule_hash),
            (
                "verifier effective definition",
                &self.effective_definition_digest,
            ),
            ("verifier exact program", &self.exact_program_hash),
            ("verifier parameters", &self.admitted_parameters_digest),
            ("verifier launch authority", &self.launch_authority_digest),
            (
                "verifier execution realization",
                &self.execution_realization_hash,
            ),
            (
                "verifier execution substrate",
                &self.substrate_identity_hash,
            ),
            ("verifier admitted subject", &self.subject_manifest_hash),
            ("verifier terminal snapshot", &self.terminal_snapshot_hash),
            ("verifier result", &self.result_digest),
        ] {
            validate_hash(label, hash)?;
        }
        Ok(())
    }

    pub fn project_root_from_closure(
        closure: &crate::objects::AdmittedExecutionClosure,
    ) -> anyhow::Result<Option<std::path::PathBuf>> {
        let root = match closure {
            crate::objects::AdmittedExecutionClosure::DirectItemExecutor {
                admitted_project_root,
                ..
            } => admitted_project_root.clone(),
            crate::objects::AdmittedExecutionClosure::ManagedRuntime { .. } => None,
        };
        if root.as_deref().is_some_and(|root| {
            root.as_os_str() != std::ffi::OsStr::new(crate::objects::ADMITTED_DIRECT_PROJECT_ROOT)
        }) {
            bail!("qualification requires the existing normalized direct execution root");
        }
        Ok(root)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationProjectorIdentity {
    pub canonical_ref: String,
    pub descriptor_content_digest: String,
    pub descriptor_signer_fingerprint: String,
    pub binary_content_digest: String,
    pub binary_manifest_digest: String,
    pub binary_signer_fingerprint: String,
}

impl ProductQualificationProjectorIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_canonical_unsuffixed_ref("qualification projector", &self.canonical_ref)?;
        for (label, hash) in [
            ("projector descriptor", &self.descriptor_content_digest),
            (
                "projector descriptor signer",
                &self.descriptor_signer_fingerprint,
            ),
            ("projector binary", &self.binary_content_digest),
            ("projector binary manifest", &self.binary_manifest_digest),
            ("projector binary signer", &self.binary_signer_fingerprint),
        ] {
            validate_hash(label, hash)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationParticipant {
    /// Opaque stable role/occurrence named by the signed projection contract.
    pub call_id: String,
    pub operation_id: String,
    pub request_hash: String,
    pub action_digest: String,
    /// Canonical digest of the exact normalized parent/child realization set.
    /// These are inherited admitted inputs, not forwarded caller selectors.
    pub inherited_realizations_digest: String,
    pub verifier: ProductQualificationVerifier,
}

impl ProductQualificationParticipant {
    fn validate(&self) -> anyhow::Result<()> {
        exact_coordinate("qualification participant call", &self.call_id)?;
        if self.call_id.len() > MAX_PRODUCT_QUALIFICATION_CALL_ID_BYTES {
            bail!("qualification participant call exceeds its byte bound");
        }
        for (label, hash) in [
            ("qualification probe operation", &self.operation_id),
            ("qualification probe request", &self.request_hash),
            ("qualification probe action", &self.action_digest),
            (
                "qualification probe inherited inputs",
                &self.inherited_realizations_digest,
            ),
        ] {
            validate_hash(label, hash)?;
        }
        self.verifier.validate()?;
        Ok(())
    }
}

/// Contract-owned interpretation of the exact executed participant set.
/// State validates mechanical identity and bounds, never a runtime's language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationExecutionProof {
    /// Declaration owner, distinct from the realization's execution contract.
    pub projection_contract_ref: String,
    pub projection_contract_digest: String,
    pub projector: ProductQualificationProjectorIdentity,
    pub participants: Vec<ProductQualificationParticipant>,
}

impl ProductQualificationExecutionProof {
    pub fn validate_for(&self, root: &ProductQualificationVerifier) -> anyhow::Result<()> {
        validate_canonical_unsuffixed_ref(
            "qualification projection contract",
            &self.projection_contract_ref,
        )?;
        validate_hash(
            "qualification projection contract digest",
            &self.projection_contract_digest,
        )?;
        self.projector.validate()?;
        let (contract_ref, contract_digest) = match &root.artifact_identity {
            AdmittedLaunchArtifactIdentity::ManagedRuntime {
                runtime_ref,
                runtime_content_hash,
                ..
            } => (runtime_ref, runtime_content_hash),
            AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                protocol_ref,
                protocol_content_hash,
                ..
            } => (protocol_ref, protocol_content_hash),
        };
        if &self.projection_contract_ref != contract_ref
            || &self.projection_contract_digest != contract_digest
        {
            bail!("qualification projection contract contradicts its admitted descriptor owner");
        }
        if self.participants.len() > MAX_PRODUCT_QUALIFICATION_PARTICIPANTS {
            bail!("qualification execution proof exceeds participant bound");
        }
        let mut calls = BTreeSet::new();
        let mut operations = BTreeSet::new();
        let mut threads = BTreeSet::from([root.thread_id.as_str()]);
        for participant in &self.participants {
            participant.validate()?;
            if !calls.insert(participant.call_id.as_str())
                || !operations.insert(participant.operation_id.as_str())
                || !threads.insert(participant.verifier.thread_id.as_str())
            {
                bail!("qualification execution proof repeats an occurrence or verifier thread");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationEvidence {
    pub schema: String,
    pub product_witness_hash: String,
    pub witness_source: super::transfer::ProductWitnessSource,
    pub product_coordinate: ProductCaptureCoordinate,
    pub policy_source: ProductQualificationPolicySource,
    pub verifier: ProductQualificationVerifier,
    pub execution_proof: ProductQualificationExecutionProof,
    /// Exact root product selections retained from the admitted verifier.
    /// `None` is the literal-pin lane. Missing is never an alias for null.
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub verifier_root_selections: Option<ResolvedExternalProductSelections>,
    pub result: ProductQualificationResult,
}

impl ProductQualificationEvidence {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        bounded(
            value,
            MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES,
            "qualification evidence",
        )?;
        let evidence: Self =
            serde_json::from_value(value.clone()).context("decode qualification evidence")?;
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PRODUCT_QUALIFICATION_EVIDENCE_SCHEMA {
            bail!("unsupported product qualification evidence schema");
        }
        validate_hash("qualified product witness", &self.product_witness_hash)?;
        self.witness_source.validate()?;
        self.product_coordinate.validate()?;
        self.policy_source.validate()?;
        self.verifier.validate()?;
        self.execution_proof.validate_for(&self.verifier)?;
        for participant in &self.execution_proof.participants {
            if participant.verifier.chain_root_id == self.product_coordinate.chain_root_id
                || participant.verifier.thread_id == self.product_coordinate.thread_id
            {
                bail!("product producer cannot be its own qualification participant");
            }
        }
        self.result
            .validate_claims_for(&self.policy_source.policy, &[])?;
        self.validate_verifier_root_selections()?;
        if self.product_coordinate.chain_root_id == self.verifier.chain_root_id
            || self.product_coordinate.thread_id == self.verifier.thread_id
        {
            bail!("product qualification requires an independent verifier chain");
        }
        if self.verifier.canonical_ref != self.policy_source.policy.verifier_ref
            || self.verifier.subject_declaration_id
                != self.policy_source.policy.subject_declaration_id
            || self.verifier.admitted_parameters_digest
                != self.policy_source.policy.admitted_parameters_digest()?
            || self.verifier.subject_manifest_hash != self.result.subject_manifest_hash
            || self.verifier.result_digest != self.result.digest()?
        {
            bail!(
                "qualification testimony contradicts its policy, admitted subject or terminal result"
            );
        }
        bounded(
            self,
            MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES,
            "qualification evidence",
        )
    }

    fn validate_verifier_root_selections(&self) -> anyhow::Result<()> {
        let Some(selections) = &self.verifier_root_selections else {
            return Ok(());
        };
        selections.validate()?;
        if selections.is_empty() {
            bail!("qualification verifier root selections must not be empty");
        }
        bounded(
            selections,
            MAX_PRODUCT_QUALIFICATION_SELECTIONS_BYTES,
            "qualification verifier root selections",
        )?;
        let subject = selections
            .get(&self.verifier.subject_declaration_id)
            .context("qualification verifier root selections omit the admitted subject")?;
        if subject.witness_hash != self.product_witness_hash
            || subject.witness_source != self.witness_source
            || subject.witness_coordinate != self.product_coordinate
            || subject.manifest_hash != self.verifier.subject_manifest_hash
            || subject.declaration_id != self.verifier.subject_declaration_id
            || subject.relationship.consumer.canonical_ref != self.verifier.canonical_ref
            || subject.relationship.consumer.declaration_id != self.verifier.subject_declaration_id
            || subject.relationship.qualification.policy_ref.is_some()
            || !subject
                .relationship
                .qualification
                .required_claims
                .is_empty()
            || subject.qualification.is_some()
        {
            bail!(
                "qualification verifier root selection contradicts its exact unqualified product subject"
            );
        }
        Ok(())
    }

    /// Exact published testimony retained by this qualification attestation.
    /// Historical verifier capsules and terminal coordinates remain non-owning.
    pub(crate) fn owning_attestation_hashes(&self) -> anyhow::Result<Vec<String>> {
        self.validate()?;
        let mut hashes = BTreeSet::from([self.product_witness_hash.clone()]);
        if let super::transfer::ProductWitnessSource::Received { acceptance_hash } =
            &self.witness_source
        {
            hashes.insert(acceptance_hash.clone());
        }
        if let Some(selections) = &self.verifier_root_selections {
            for (_, selection) in selections.iter() {
                hashes.insert(selection.witness_hash.clone());
                if let super::transfer::ProductWitnessSource::Received { acceptance_hash } =
                    &selection.witness_source
                {
                    hashes.insert(acceptance_hash.clone());
                }
                if let Some(qualification) = &selection.qualification {
                    hashes.insert(qualification.attestation_hash.clone());
                }
            }
        }
        Ok(hashes.into_iter().collect())
    }

    /// Semantic testimony keeps every fact except local receipt redemption.
    /// Full attestation bytes and the exact attestation hash remain retained.
    pub fn semantic_identity_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        let Self {
            schema,
            product_witness_hash,
            witness_source: _,
            product_coordinate,
            policy_source,
            verifier,
            execution_proof,
            verifier_root_selections,
            result,
        } = self;
        #[derive(Serialize)]
        struct Identity<'a> {
            schema: &'a str,
            product_witness_hash: &'a str,
            product_coordinate: &'a ProductCaptureCoordinate,
            policy_source: &'a ProductQualificationPolicySource,
            verifier: &'a ProductQualificationVerifier,
            execution_proof: &'a ProductQualificationExecutionProof,
            verifier_root_selections: Option<Value>,
            result: &'a ProductQualificationResult,
        }
        let verifier_root_selections = verifier_root_selections
            .as_ref()
            .map(ResolvedExternalProductSelections::semantic_identity_value)
            .transpose()?;
        Ok(serde_json::to_value(Identity {
            schema,
            product_witness_hash,
            product_coordinate,
            policy_source,
            verifier,
            execution_proof,
            verifier_root_selections,
            result,
        })?)
    }

    pub(crate) fn execution_verifiers(
        &self,
    ) -> impl Iterator<Item = &ProductQualificationVerifier> {
        std::iter::once(&self.verifier).chain(
            self.execution_proof
                .participants
                .iter()
                .map(|participant| &participant.verifier),
        )
    }

    /// Current-source eligibility only. The app must additionally authenticate
    /// the product witness and issuer, check expiry, and prove that these exact
    /// current source identities were resolved under the caller's authority.
    pub fn validate_current_policy(
        &self,
        current_policy: &ProductQualificationPolicySource,
        current_verifier_effective_definition_digest: &str,
        required_claims: &[String],
    ) -> anyhow::Result<()> {
        self.validate()?;
        current_policy.validate()?;
        validate_hash(
            "current qualification verifier",
            current_verifier_effective_definition_digest,
        )?;
        if &self.policy_source != current_policy
            || self.verifier.effective_definition_digest
                != current_verifier_effective_definition_digest
        {
            bail!("qualification no longer matches the exact current policy and verifier");
        }
        self.result
            .validate_claims_for(&current_policy.policy, required_claims)
    }

    /// Fresh admission must re-establish executable/protocol/runtime identity
    /// as well as the effective item definition. Recovery uses the retained
    /// realization instead; it must not substitute today's installed code.
    pub fn validate_current_artifact(
        &self,
        current: &AdmittedLaunchArtifactIdentity,
    ) -> anyhow::Result<()> {
        self.validate()?;
        current.validate()?;
        if &self.verifier.artifact_identity != current {
            bail!("qualification no longer matches the exact current verifier runtime artifact");
        }
        Ok(())
    }

    pub fn sign_attestation(
        &self,
        signer: &dyn Signer,
        issued_at: String,
        expires_at: Option<String>,
    ) -> anyhow::Result<Attestation> {
        self.validate()?;
        Attestation::unsigned(
            self.result.subject_manifest_hash.clone(),
            PRODUCT_QUALIFICATION_CLAIM.into(),
            PRODUCT_QUALIFICATION_ATTESTATION_POLICY.into(),
            issued_at,
            expires_at,
            serde_json::to_value(self)?,
        )
        .sign(signer)
    }

    /// Structural decoding is not proof of successful execution or authority.
    pub fn from_attestation(attestation: &Attestation) -> anyhow::Result<Self> {
        attestation.validate()?;
        if attestation.claim != PRODUCT_QUALIFICATION_CLAIM
            || attestation.policy != PRODUCT_QUALIFICATION_ATTESTATION_POLICY
        {
            bail!("attestation is not product qualification testimony");
        }
        let evidence = Self::from_value(&attestation.evidence)?;
        if attestation.subject_hash != evidence.result.subject_manifest_hash {
            bail!("qualification attestation subject disagrees with admitted verifier subject");
        }
        Ok(evidence)
    }

    /// Exact node/owner authentication. Expiry and current-source eligibility
    /// are intentionally separate admission duties, not ignored permissions.
    pub fn verify_attestation_for_owner(
        attestation: &Attestation,
        node_key: &lillux::crypto::VerifyingKey,
        owner_principal: &str,
    ) -> anyhow::Result<Self> {
        attestation.verify_with_key(node_key)?;
        let evidence = Self::from_attestation(attestation)?;
        if evidence.product_coordinate.owner_principal != owner_principal {
            bail!("qualification testimony belongs to another product owner");
        }
        Ok(evidence)
    }
}

fn validate_claims(claims: &[String]) -> anyhow::Result<()> {
    if claims.is_empty() {
        bail!("qualification claims must not be empty");
    }
    validate_claims_allow_empty(claims)
}

fn validate_claims_allow_empty(claims: &[String]) -> anyhow::Result<()> {
    if claims.len() > MAX_PRODUCT_QUALIFICATION_CLAIMS
        || claims.windows(2).any(|pair| pair[0] >= pair[1])
    {
        bail!("qualification claims must be a bounded sorted unique set");
    }
    for claim in claims {
        validate_name(claim)?;
    }
    Ok(())
}

fn bounded_object(value: &Value, label: &str) -> anyhow::Result<()> {
    if !value.is_object() {
        bail!("{label} must be an object");
    }
    bounded(value, MAX_PROBE_VALUE_BYTES, label)
}

fn bounded(value: &impl Serialize, maximum: usize, label: &str) -> anyhow::Result<()> {
    let value = serde_json::to_value(value)?;
    if lillux::canonical_json(&value)?.len() > maximum {
        bail!("{label} exceeds its bounded wire contract");
    }
    Ok(())
}

fn exact_coordinate(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 2048
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        bail!("{label} must be an exact bounded coordinate");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
