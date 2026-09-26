//! Finite product qualification policy and compact node testimony.
//!
//! These types do not execute a verifier or authorize its claims. The app
//! proves an independently completed admitted verifier, including its actual
//! subject realization, before signing. Attestation's existing subject edge
//! retains the product manifest, while the typed qualification contract retains
//! its exact product/qualification witnesses. Historical program and terminal
//! coordinates remain non-owning node testimony, not independent full-run proof.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::composition::{ProductRelationshipConsumer, ResolvedExternalProductSelections};
use super::publication::ProductCaptureCoordinate;
use super::transfer::ProductWitnessSource;
use super::{validate_canonical_unsuffixed_ref, validate_hash, validate_name};
use crate::Signer;
use crate::objects::{AdmittedLaunchArtifactIdentity, Attestation, canonical_value_digest};

pub const PRODUCT_QUALIFICATION_POLICY_SCHEMA: &str = "ryeos.product_qualification_policy.v2";
pub const PRODUCT_QUALIFICATION_LAUNCH_PURPOSE_SCHEMA: &str =
    "ryeos.product_qualification_launch_purpose.v1";
pub const PRODUCT_QUALIFICATION_RESULT_SCHEMA: &str = "ryeos.product_qualification_result.v1";
pub const PRODUCT_QUALIFICATION_EVIDENCE_SCHEMA: &str = "ryeos.product_qualification_evidence.v8";
pub const PRODUCT_QUALIFICATION_ATTESTATION_POLICY: &str = "ryeos.product_qualification.v1";
pub const PRODUCT_QUALIFICATION_CLAIM: &str = "retained_product_qualified";
pub const MAX_PRODUCT_QUALIFICATION_CLAIMS: usize = 32;
pub const MAX_PRODUCT_QUALIFICATION_POLICY_BYTES: usize = 16 * 1024;
pub const MAX_PRODUCT_QUALIFICATION_RESULT_BYTES: usize = 16 * 1024;
pub const MAX_PRODUCT_QUALIFICATION_SELECTIONS_BYTES: usize = 16 * 1024;
pub const MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES: usize = 64 * 1024;
pub const MAX_PRODUCT_QUALIFICATION_PARTICIPANTS: usize = 16;
pub const MAX_PRODUCT_QUALIFICATION_PRODUCER_SCENARIOS: usize = 8;
pub const MAX_PRODUCT_QUALIFICATION_CALL_ID_BYTES: usize = 128;
const MAX_PROBE_VALUE_BYTES: usize = 8 * 1024;

/// What the node actually proved about a direct verifier after its target
/// was reaped. A trusted process group is not whole-descendant containment:
/// a child may create another session. Signed policy must explicitly allow
/// that weaker controller lane; hard claims require a scoped observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifierProcessSettlementAuthority {
    ScopeEmpty,
    TrustedProcessGroupAbsent,
}

impl VerifierProcessSettlementAuthority {
    fn satisfies(self, minimum: Self) -> bool {
        self == minimum || (self == Self::ScopeEmpty && minimum == Self::TrustedProcessGroupAbsent)
    }
}

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
    pub minimum_verifier_process_settlement: VerifierProcessSettlementAuthority,
    pub verifier_parameters: Value,
    /// Signed consumer closure that a runtime-qualification verifier must
    /// exercise. This declaration is not itself evidence of admission: the
    /// launch and proof owners must resolve these refs from one checked
    /// Bundle generation and join them to the selected relationship.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_execution_context: Option<ProductQualificationConsumerExecutionContext>,
    /// Finite signed alternatives for one selected producer attempt per
    /// admitted verifier root. A ref is only source identity, not permission
    /// to launch: the daemon must resolve and admit a typed Bundle recipe.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub producer_scenarios: BTreeMap<String, ProductQualificationProducerScenario>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationConsumerExecutionContext {
    pub worker_ref: String,
    pub product_declaration_id: String,
    pub environment_ref: String,
    pub worker_execution_ref: String,
    pub environment_binding: String,
}

impl ProductQualificationConsumerExecutionContext {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (label, reference, kind) in [
            ("qualification consumer worker", &self.worker_ref, "worker:"),
            (
                "qualification consumer environment",
                &self.environment_ref,
                "config:",
            ),
            (
                "qualification consumer worker execution",
                &self.worker_execution_ref,
                "worker_execution:",
            ),
        ] {
            validate_canonical_unsuffixed_ref(label, reference)?;
            if !reference.starts_with(kind) {
                bail!("{label} must be a {kind} ref");
            }
        }
        validate_name(&self.product_declaration_id)?;
        validate_name(&self.environment_binding)
    }

    pub fn validate_relationship_consumer(
        &self,
        consumer: &ProductRelationshipConsumer,
    ) -> anyhow::Result<()> {
        self.validate()?;
        consumer.validate()?;
        if self.worker_ref != consumer.canonical_ref
            || self.product_declaration_id != consumer.declaration_id
        {
            bail!("qualification consumer context differs from signed product relationship");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationProducerScenario {
    pub recipe_ref: String,
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
        if let Some(context) = &self.consumer_execution_context {
            context.validate()?;
        }
        if self.producer_scenarios.len() > MAX_PRODUCT_QUALIFICATION_PRODUCER_SCENARIOS {
            bail!("qualification producer scenario count exceeds bound");
        }
        for (name, scenario) in &self.producer_scenarios {
            validate_name(name)?;
            validate_canonical_unsuffixed_ref(
                "qualification producer recipe",
                &scenario.recipe_ref,
            )?;
            if !scenario.recipe_ref.starts_with("config:") {
                bail!("qualification producer recipe must be a Config ref");
            }
        }
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

/// Daemon-derived intent for one independently admitted verifier root. This
/// is not a caller-authored declaration or qualification result: the launch
/// owner must derive it from the exact product witness and signed relationship
/// before attaching it to root admission. Recovery retains it unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationLaunchPurpose {
    pub schema: String,
    pub launch_id: String,
    pub owner_fingerprint: String,
    pub product_witness_hash: String,
    pub witness_source: ProductWitnessSource,
    pub relationship_name: String,
    pub policy_source: ProductQualificationPolicySource,
    /// Exact signed source for every scenario the verifier may later select.
    /// A Config ref alone cannot prevent recipe drift after root admission.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub producer_recipe_sources: BTreeMap<String, ProductProducerRecipeSourceIdentity>,
    pub subject_declaration_id: String,
    pub subject_manifest_hash: String,
    pub required_claims: Vec<String>,
    pub admitted_parameters_digest: String,
    pub verifier_ref: String,
    /// Pre-realization effective definition admitted for the verifier root.
    /// The capsule's realized D2 remains separately authoritative.
    pub verifier_effective_definition_digest: String,
    /// Exact current post-realization definition independently resolved from
    /// the signed Bundle before launch. It must match the retained capsule's
    /// realized definition at qualification; it is not interchangeable with
    /// the root admission's pre-realization definition above.
    pub verifier_realized_definition_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductProducerRecipeSourceIdentity {
    pub bundle_generation_identity: String,
    pub canonical_ref: String,
    pub raw_content_digest: String,
    pub effective_definition_digest: String,
    pub publisher_fingerprint: String,
    pub recipe_digest: String,
}

impl ProductProducerRecipeSourceIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        exact_coordinate(
            "producer recipe Bundle generation",
            &self.bundle_generation_identity,
        )?;
        validate_canonical_unsuffixed_ref("producer recipe", &self.canonical_ref)?;
        if !self.canonical_ref.starts_with("config:") {
            bail!("producer recipe source must be a Config");
        }
        validate_hash("producer recipe source", &self.raw_content_digest)?;
        validate_hash(
            "producer recipe definition",
            &self.effective_definition_digest,
        )?;
        validate_hash("producer recipe publisher", &self.publisher_fingerprint)?;
        validate_hash("producer recipe contents", &self.recipe_digest)
    }
}

impl ProductQualificationLaunchPurpose {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PRODUCT_QUALIFICATION_LAUNCH_PURPOSE_SCHEMA {
            bail!("unsupported product qualification launch purpose schema");
        }
        if self.launch_id.len() != 34
            || !self.launch_id.starts_with("L-")
            || !self.launch_id[2..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("qualification launch id is not canonical");
        }
        exact_coordinate("qualification launch owner", &self.owner_fingerprint)?;
        validate_hash("qualification product witness", &self.product_witness_hash)?;
        self.witness_source.validate()?;
        validate_name(&self.relationship_name)?;
        self.policy_source.validate()?;
        if self.producer_recipe_sources.len() != self.policy_source.policy.producer_scenarios.len()
        {
            bail!("qualification purpose does not pin every signed producer scenario");
        }
        for (name, scenario) in &self.policy_source.policy.producer_scenarios {
            let source = self
                .producer_recipe_sources
                .get(name)
                .context("qualification purpose has no producer source for signed scenario")?;
            source.validate()?;
            if source.canonical_ref != scenario.recipe_ref {
                bail!("qualification purpose producer source differs from signed scenario");
            }
        }
        validate_name(&self.subject_declaration_id)?;
        validate_hash(
            "qualification subject manifest",
            &self.subject_manifest_hash,
        )?;
        validate_claims(&self.required_claims)?;
        validate_hash(
            "qualification admitted parameters",
            &self.admitted_parameters_digest,
        )?;
        validate_canonical_unsuffixed_ref("qualification verifier", &self.verifier_ref)?;
        validate_hash(
            "qualification verifier definition",
            &self.verifier_effective_definition_digest,
        )?;
        validate_hash(
            "qualification realized verifier definition",
            &self.verifier_realized_definition_digest,
        )?;
        let policy = &self.policy_source.policy;
        if self.subject_declaration_id != policy.subject_declaration_id
            || self.verifier_ref != policy.verifier_ref
            || self.admitted_parameters_digest != policy.admitted_parameters_digest()?
            || self
                .required_claims
                .iter()
                .any(|claim| policy.allowed_claims.binary_search(claim).is_err())
        {
            bail!("qualification launch purpose contradicts its signed policy");
        }
        bounded(
            self,
            MAX_PRODUCT_QUALIFICATION_EVIDENCE_BYTES,
            "qualification launch purpose",
        )
    }
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
        // The typed declaration alone is not an admitted Worker closure.
        // Until launch, proof, and fresh selection all authenticate the same
        // retained consumer context, no claim may be issued under this field.
        if policy.consumer_execution_context.is_some() {
            bail!("qualification consumer execution context has no authenticated closure proof");
        }
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
    /// Daemon-authored signed-terminal bridge to an exact settled process
    /// attempt. Direct subprocess verifiers require it; managed roots have
    /// no direct subprocess and use their independently proved participants.
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub process_settlement_witness_digest: Option<String>,
    /// Explicit strength of the signed process-settlement witness. A direct
    /// verifier has exactly one; managed roots have none and prove direct
    /// participants independently.
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub process_settlement_authority: Option<VerifierProcessSettlementAuthority>,
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
        match &self.artifact_identity {
            AdmittedLaunchArtifactIdentity::DirectItemExecutor { .. } => {
                validate_hash(
                    "qualification direct process settlement",
                    self.process_settlement_witness_digest
                        .as_deref()
                        .ok_or_else(|| {
                            anyhow::anyhow!("direct verifier lacks process settlement")
                        })?,
                )?;
                if self.process_settlement_authority.is_none() {
                    bail!("direct verifier lacks typed process settlement authority");
                }
            }
            AdmittedLaunchArtifactIdentity::ManagedRuntime { .. }
                if self.process_settlement_witness_digest.is_some()
                    || self.process_settlement_authority.is_some() =>
            {
                bail!("managed verifier cannot borrow a direct process settlement");
            }
            _ => {}
        }
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
pub struct ProductQualificationScopedAttemptProof {
    pub attempt_id: String,
    pub launch_owner_digest: String,
    pub scenario_id: String,
    pub producer_source: ProductProducerRecipeSourceIdentity,
    pub process_identity_digest: String,
    pub scope_allocation_digest: String,
    pub scope_recovery_digest: String,
    pub mount_preparation_digest: String,
    pub natural_empty_receipt_digest: String,
    pub observation_object_hash: String,
    pub recovery_death_evidence_digest: String,
    pub retirement_evidence_digest: String,
    pub callback_method_surface_digest: String,
}

impl ProductQualificationScopedAttemptProof {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.attempt_id.len() != 71
            || !self.attempt_id.starts_with("scoped-")
            || !self.attempt_id[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("qualification scoped attempt id is not canonical");
        }
        validate_name(&self.scenario_id)?;
        self.producer_source.validate()?;
        for (label, digest) in [
            ("scoped launch owner", &self.launch_owner_digest),
            ("scoped process identity", &self.process_identity_digest),
            ("scoped allocation", &self.scope_allocation_digest),
            ("scoped recovery", &self.scope_recovery_digest),
            ("scoped mount preparation", &self.mount_preparation_digest),
            (
                "scoped natural-empty receipt",
                &self.natural_empty_receipt_digest,
            ),
            ("scoped observation", &self.observation_object_hash),
            (
                "scoped recovery death",
                &self.recovery_death_evidence_digest,
            ),
            ("scoped retirement", &self.retirement_evidence_digest),
            (
                "scoped callback method surface",
                &self.callback_method_surface_digest,
            ),
        ] {
            validate_hash(label, digest)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationExecutionProof {
    /// Declaration owner, distinct from the realization's execution contract.
    pub projection_contract_ref: String,
    pub projection_contract_digest: String,
    pub projector: ProductQualificationProjectorIdentity,
    pub participants: Vec<ProductQualificationParticipant>,
    /// Required nullable: a scoped producer is a daemon-owned process attempt,
    /// never a synthetic managed child participant.
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub scoped_attempt: Option<ProductQualificationScopedAttemptProof>,
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
        if let Some(scoped) = &self.scoped_attempt {
            scoped.validate()?;
            if !self.participants.is_empty() {
                bail!("qualification cannot mix scoped attempt and managed participants");
            }
            if !matches!(
                root.artifact_identity,
                AdmittedLaunchArtifactIdentity::DirectItemExecutor { .. }
            ) {
                bail!("qualification scoped attempt requires a direct verifier");
            }
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
        if let Some(scoped) = &self.execution_proof.scoped_attempt {
            let scenario = self
                .policy_source
                .policy
                .producer_scenarios
                .get(&scoped.scenario_id)
                .context("qualification scoped attempt has no signed producer scenario")?;
            if scenario.recipe_ref != scoped.producer_source.canonical_ref {
                bail!("qualification scoped attempt recipe differs from signed scenario");
            }
        }
        for verifier in self.execution_verifiers() {
            if let Some(actual) = verifier.process_settlement_authority
                && !actual.satisfies(
                    self.policy_source
                        .policy
                        .minimum_verifier_process_settlement,
                )
            {
                bail!("qualification verifier settlement is weaker than signed policy");
            }
        }
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

#[cfg(any(test, feature = "test-support"))]
pub(crate) mod tests;

/// Exact qualification fixtures for cross-crate composed tests. Production
/// builds cannot name this surface; callers must explicitly enable the
/// repository's `test-support` feature.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::super::composition::ResolvedExternalProductSelections;

    pub fn qualified_runtime_selections(
        runtime_manifest_hash: &str,
    ) -> anyhow::Result<ResolvedExternalProductSelections> {
        let evidence = super::tests::dynamic_evidence_with_auxiliary_proof();
        let mut selections = evidence
            .verifier_root_selections
            .expect("qualified fixture retains verifier-root selections")
            .into_inner();
        let runtime = selections
            .get_mut("auxiliary")
            .expect("qualified fixture retains auxiliary runtime");
        runtime.manifest_hash = runtime_manifest_hash.to_owned();
        runtime.declaration.manifest_hash = runtime_manifest_hash.to_owned();
        let qualification = runtime
            .qualification
            .as_mut()
            .expect("qualified fixture retains runtime proof");
        qualification.evidence.result.subject_manifest_hash = runtime_manifest_hash.to_owned();
        qualification.evidence.verifier.subject_manifest_hash = runtime_manifest_hash.to_owned();
        qualification.evidence.verifier.result_digest = qualification.evidence.result.digest()?;
        ResolvedExternalProductSelections::new(selections)
    }

    pub fn launch_artifact_identity() -> crate::objects::AdmittedLaunchArtifactIdentity {
        super::tests::artifact_identity()
    }
}
