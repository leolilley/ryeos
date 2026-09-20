//! Read-only consumer resolution, verification, fetch, and deployment planning.
//!
//! These operations deliberately produce immutable coordinates and plans. They
//! neither write the installed bundle registry nor authorize activation.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};
use ryeos_bundle_publication_contract::{
    BundleCatalogPublication, BundleCatalogSnapshot, BundleGeneration, BundleSet, BundleTarget,
    MigrationDecision, MigrationRequirement, NodeBundleSelection, SubstrateRelease,
};
use ryeos_state::objects::Attestation;
use serde_json::Value;
use std::sync::Arc;

use super::{
    BundleReleaseEvidenceProof, PublicationObjectReader, PublisherMaterializationProof,
    ReleasePolicyBinding, VerifiedBundleGeneration,
    attestation::{
        BUNDLE_CATALOG_RELEASE_CLAIM, BUNDLE_GENERATION_RELEASE_CLAIM, BUNDLE_PUBLICATION_POLICY,
        BUNDLE_SET_RELEASE_CLAIM, SUBSTRATE_RELEASE_CLAIM,
    },
    verify_bundle_generation,
};

pub const DEPLOYMENT_SELECTION_CLAIM: &str = "ryeos.node-bundle-selection.deploy.v1";
pub const BUNDLE_DEPLOYMENT_POLICY: &str = "ryeos.bundle-deployment.v1";
pub const CORE_BUNDLE_NAME: &str = "core";

/// Concrete proof authority for a remotely fetched closure. Reusing the
/// standalone publisher verifier is intentional: producer and consumer must
/// apply the same qualification, publisher-tool, closed-mutation, and policy
/// bindings to identical CAS bytes.
pub struct FetchedCasReleaseProof {
    inner: super::standalone_publisher::StandalonePublisherProof,
}

impl FetchedCasReleaseProof {
    pub fn from_current_policy(
        cas: Arc<lillux::CasStore>,
        policy: super::standalone_publisher::StandalonePublisherPolicy,
        binding: &ReleasePolicyBinding,
    ) -> anyhow::Result<Self> {
        policy.validate()?;
        if policy.catalog_namespace != binding.catalog_namespace
            || policy.bundle_publication_policy_section_digest
                != binding.bundle_publication_policy_section_digest
            || policy.trust_epoch != binding.trust_epoch
        {
            bail!("fetched-CAS verifier policy differs from current consumer binding");
        }
        Ok(Self {
            inner: super::standalone_publisher::StandalonePublisherProof::new(cas, policy)?,
        })
    }
}

impl PublisherMaterializationProof for FetchedCasReleaseProof {
    fn verify_closed_mutation(
        &self,
        result: &ryeos_bundle_publication_contract::PublisherMaterializationResult,
        input: &ryeos_state::objects::ExternalContentManifestObject,
        output: &ryeos_state::objects::ExternalContentManifestObject,
    ) -> anyhow::Result<()> {
        self.inner.verify_closed_mutation(result, input, output)
    }
}

impl BundleReleaseEvidenceProof for FetchedCasReleaseProof {
    fn verify_release_evidence(
        &self,
        generation: &BundleGeneration,
        accepted: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        accepted_capture: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        materialization: &ryeos_bundle_publication_contract::PublisherMaterializationResult,
        binding: &ReleasePolicyBinding,
    ) -> anyhow::Result<()> {
        self.inner.verify_release_evidence(
            generation,
            accepted,
            accepted_capture,
            materialization,
            binding,
        )
    }

    fn verify_substrate_release_evidence(
        &self,
        release: &SubstrateRelease,
        accepted_result: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        policy_binding: &ReleasePolicyBinding,
    ) -> anyhow::Result<()> {
        self.inner
            .verify_substrate_release_evidence(release, accepted_result, policy_binding)
    }
}

/// Local policy and cryptographic authority. Structural decoding alone never
/// creates one of the verified consumer values below.
pub trait ConsumerPublicationPolicy: Send + Sync {
    fn verify_publisher_attestation(
        &self,
        attestation: &Attestation,
        expected_claim: &str,
    ) -> anyhow::Result<()>;

    fn verify_deployment_attestation(&self, attestation: &Attestation) -> anyhow::Result<()>;
}

/// Offline cryptographic authority compiled from one verified node-policy
/// generation and the node's pinned item-signer trust store. It retains exact
/// keys, rather than consulting mutable trust during an update transaction.
pub struct CurrentConsumerPublicationPolicy {
    namespace: String,
    publisher_fingerprint: String,
    publisher_key: lillux::crypto::VerifyingKey,
    generation_claim: String,
    set_claim: String,
    catalog_claim: String,
    publication_policy: String,
    operator_fingerprint: String,
    operator_key: lillux::crypto::VerifyingKey,
    release_proof_policy: super::standalone_publisher::StandalonePublisherPolicy,
}

impl CurrentConsumerPublicationPolicy {
    pub fn from_verified_snapshot(
        snapshot: &crate::node_policy::NodePolicySnapshot,
        trust_store: &ryeos_engine::trust::TrustStore,
        namespace: &str,
        operator_fingerprint: &str,
    ) -> anyhow::Result<Self> {
        let policy = snapshot
            .require::<crate::node_policy::sections::bundle_publication::BundlePublicationPolicy>(
        )?;
        policy.validate()?;
        let catalog = policy.require_catalog(namespace)?;
        if catalog.frozen {
            bail!("catalog namespace `{namespace}` is frozen for consumer admission");
        }
        let publisher = trust_store
            .get(&catalog.publisher_fingerprint)
            .with_context(|| format!("catalog publisher for `{namespace}` is not pinned"))?;
        if lillux::crypto::fingerprint(&publisher.verifying_key) != catalog.publisher_fingerprint {
            bail!("pinned catalog publisher key has an inconsistent fingerprint");
        }
        let operator = trust_store
            .get(operator_fingerprint)
            .context("deployment operator is not pinned")?;
        if lillux::crypto::fingerprint(&operator.verifying_key) != operator_fingerprint {
            bail!("pinned deployment operator key has an inconsistent fingerprint");
        }
        let release_proof_policy = super::standalone_publisher::StandalonePublisherPolicy {
            core_seed_qualification_policy: catalog.core_seed_qualification_policy.clone(),
            core_seed_qualification_verifier_effective_definition_digest: catalog
                .core_seed_qualification_verifier_effective_definition_digest
                .clone(),
            core_seed_qualification_verifier_artifact_identity: catalog
                .core_seed_qualification_verifier_artifact_identity
                .clone(),
            required_core_seed_qualification_claims: catalog
                .required_core_seed_qualification_claims
                .clone(),
            schema: "ryeos.standalone_bundle_publisher_policy.v1".to_owned(),
            catalog_namespace: namespace.to_owned(),
            catalog_publisher_fingerprint: catalog.publisher_fingerprint.clone(),
            bundle_publication_policy_section_digest: policy.section_digest()?,
            trust_epoch: catalog.trust_epoch,
            qualification_signer_public_key: catalog.qualification_signer_public_key,
            qualification_signer_fingerprint: catalog.qualification_signer_fingerprint.clone(),
            qualification_policy: catalog.qualification_policy.clone(),
            qualification_verifier_effective_definition_digest: catalog
                .qualification_verifier_effective_definition_digest
                .clone(),
            qualification_verifier_artifact_identity: catalog
                .qualification_verifier_artifact_identity
                .clone(),
            required_qualification_claims: catalog.required_qualification_claims.clone(),
            substrate_qualification_policy: catalog.substrate_qualification_policy.clone(),
            substrate_qualification_verifier_effective_definition_digest: catalog
                .substrate_qualification_verifier_effective_definition_digest
                .clone(),
            substrate_qualification_verifier_artifact_identity: catalog
                .substrate_qualification_verifier_artifact_identity
                .clone(),
            required_substrate_qualification_claims: catalog
                .required_substrate_qualification_claims
                .clone(),
            substrate_build_signer_public_key: catalog.substrate_build_signer_public_key,
            substrate_build_signer_fingerprint: catalog.substrate_build_signer_fingerprint.clone(),
            publisher_tool_effective_definition_digest: catalog
                .publisher_tool_effective_definition_digest
                .clone(),
            publisher_tool_artifact_identity_hash: catalog
                .publisher_tool_artifact_identity_hash
                .clone(),
        };
        release_proof_policy.validate()?;
        Ok(Self {
            namespace: namespace.to_owned(),
            publisher_fingerprint: catalog.publisher_fingerprint.clone(),
            publisher_key: publisher.verifying_key.clone(),
            generation_claim: catalog.generation_claim.clone(),
            set_claim: catalog.set_claim.clone(),
            catalog_claim: catalog.catalog_publication_claim.clone(),
            publication_policy: catalog.policy.clone(),
            operator_fingerprint: operator_fingerprint.to_owned(),
            operator_key: operator.verifying_key.clone(),
            release_proof_policy,
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn release_proof_policy(&self) -> &super::standalone_publisher::StandalonePublisherPolicy {
        &self.release_proof_policy
    }

    pub fn fetched_cas_release_proof(
        &self,
        cas: Arc<lillux::CasStore>,
    ) -> anyhow::Result<FetchedCasReleaseProof> {
        FetchedCasReleaseProof::from_current_policy(
            cas,
            self.release_proof_policy.clone(),
            &ReleasePolicyBinding {
                catalog_namespace: self.release_proof_policy.catalog_namespace.clone(),
                bundle_publication_policy_section_digest: self
                    .release_proof_policy
                    .bundle_publication_policy_section_digest
                    .clone(),
                trust_epoch: self.release_proof_policy.trust_epoch,
            },
        )
    }

    fn require_current_claim(&self, claim: &str) -> anyhow::Result<()> {
        if claim != self.generation_claim
            && claim != self.set_claim
            && claim != self.catalog_claim
            && claim != SUBSTRATE_RELEASE_CLAIM
        {
            bail!("publisher attestation claim is not authorized for this catalog");
        }
        Ok(())
    }
}

impl ConsumerPublicationPolicy for CurrentConsumerPublicationPolicy {
    fn verify_publisher_attestation(
        &self,
        attestation: &Attestation,
        expected_claim: &str,
    ) -> anyhow::Result<()> {
        self.require_current_claim(expected_claim)?;
        if attestation.claim != expected_claim
            || attestation.policy != self.publication_policy
            || attestation.expires_at.is_some()
            || attestation.issuer_fingerprint()? != self.publisher_fingerprint
        {
            bail!("publisher attestation violates current catalog authorization");
        }
        attestation
            .verify_with_key(&self.publisher_key)
            .context("verify current catalog publisher attestation")
    }

    fn verify_deployment_attestation(&self, attestation: &Attestation) -> anyhow::Result<()> {
        if attestation.claim != DEPLOYMENT_SELECTION_CLAIM
            || attestation.policy != BUNDLE_DEPLOYMENT_POLICY
            || attestation.expires_at.is_some()
            || attestation.issuer_fingerprint()? != self.operator_fingerprint
        {
            bail!("deployment attestation violates explicit operator authorization");
        }
        attestation
            .verify_with_key(&self.operator_key)
            .context("verify explicit deployment operator signature")
    }
}

#[derive(Debug)]
pub struct VerifiedConsumerSet {
    set_hash: String,
    set_attestation_hash: String,
    set: BundleSet,
    substrate_release: SubstrateRelease,
    generations: BTreeMap<String, VerifiedBundleGeneration>,
}

impl VerifiedConsumerSet {
    pub fn set_hash(&self) -> &str {
        &self.set_hash
    }
    pub fn set_attestation_hash(&self) -> &str {
        &self.set_attestation_hash
    }
    pub fn set(&self) -> &BundleSet {
        &self.set
    }
    pub fn substrate_release(&self) -> &SubstrateRelease {
        &self.substrate_release
    }
    pub fn generations(&self) -> &BTreeMap<String, VerifiedBundleGeneration> {
        &self.generations
    }
}

/// Exact immutable result of resolving one set channel. Fetch and deployment
/// consume this coordinate, never the floating channel tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSetCoordinate {
    pub catalog_publication_attestation_hash: String,
    pub catalog_publication_hash: String,
    pub catalog_snapshot_hash: String,
    pub set_attestation_hash: String,
    pub set_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleFetchRequest {
    pub bundle_name: String,
    pub generation_hash: String,
    pub generation_attestation_hash: String,
    pub content_manifest_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerFetchPlan {
    pub set_attestation_hash: String,
    pub set_hash: String,
    pub requests: Vec<BundleFetchRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExactSetAction {
    Add,
    Replace,
    Remove,
    Keep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactSetPlanEntry {
    pub bundle_name: String,
    pub action: ExactSetAction,
    pub installed_generation_hash: Option<String>,
    pub selected_generation_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactSetPlan {
    pub set_attestation_hash: String,
    pub set_hash: String,
    pub entries: Vec<ExactSetPlanEntry>,
    pub requires_stopped_node: bool,
}

#[derive(Debug)]
pub struct AdmittedNodeBundleSelection {
    selection_hash: String,
    authorization_hash: String,
    selection: NodeBundleSelection,
}

impl AdmittedNodeBundleSelection {
    pub fn selection_hash(&self) -> &str {
        &self.selection_hash
    }
    pub fn authorization_hash(&self) -> &str {
        &self.authorization_hash
    }
    pub fn selection(&self) -> &NodeBundleSelection {
        &self.selection
    }
}

pub struct SelectionAdmissionContext<'a> {
    pub target_identity: &'a str,
    pub substrate_image_digest: &'a str,
    pub substrate_protocol: u32,
    pub bundle_target: &'a BundleTarget,
    pub publication_policy_section_digest: &'a str,
    pub node_policy_generation_digest: &'a str,
    pub active_selection: Option<&'a str>,
}

pub fn resolve_set_channel(
    publication_attestation_hash: &str,
    catalog_namespace: &str,
    set_name: &str,
    channel: &str,
    objects: &impl PublicationObjectReader,
    policy: &(impl ConsumerPublicationPolicy + ?Sized),
) -> anyhow::Result<ResolvedSetCoordinate> {
    let publication_attestation = read_attestation(objects, publication_attestation_hash)?;
    verify_publisher_attestation(
        policy,
        &publication_attestation,
        BUNDLE_CATALOG_RELEASE_CLAIM,
    )?;
    let publication_hash = publication_attestation.subject_hash.clone();
    let publication =
        BundleCatalogPublication::from_current_value(&read_exact(objects, &publication_hash)?)?;
    if publication.catalog_namespace != catalog_namespace {
        bail!("catalog publication belongs to a different namespace");
    }
    let snapshot = BundleCatalogSnapshot::from_current_value(&read_exact(
        objects,
        &publication.snapshot_hash,
    )?)?;
    if publication_attestation.issuer != snapshot.publisher {
        bail!("catalog publication issuer and snapshot publisher disagree");
    }
    let selected = snapshot
        .set_channels
        .iter()
        .find(|entry| entry.set_name == set_name && entry.channel == channel)
        .context("catalog snapshot has no exact requested set channel")?;
    let set_attestation = read_attestation(objects, &selected.set_attestation_hash)?;
    verify_publisher_attestation(policy, &set_attestation, BUNDLE_SET_RELEASE_CLAIM)?;
    if set_attestation.issuer != snapshot.publisher {
        bail!("curated set issuer and catalog publisher disagree");
    }
    Ok(ResolvedSetCoordinate {
        catalog_publication_attestation_hash: publication_attestation_hash.into(),
        catalog_publication_hash: publication_hash,
        catalog_snapshot_hash: publication.snapshot_hash,
        set_attestation_hash: selected.set_attestation_hash.clone(),
        set_hash: set_attestation.subject_hash,
    })
}

/// Verify a caller-supplied immutable coordinate without reintroducing a
/// mutable set/channel lookup. Every edge must be present in the exact signed
/// catalog publication and snapshot.
pub fn verify_exact_coordinate(
    coordinate: &ResolvedSetCoordinate,
    catalog_namespace: &str,
    objects: &impl PublicationObjectReader,
    policy: &(impl ConsumerPublicationPolicy + ?Sized),
) -> anyhow::Result<()> {
    let publication_attestation =
        read_attestation(objects, &coordinate.catalog_publication_attestation_hash)?;
    verify_publisher_attestation(
        policy,
        &publication_attestation,
        BUNDLE_CATALOG_RELEASE_CLAIM,
    )?;
    if publication_attestation.subject_hash != coordinate.catalog_publication_hash {
        bail!("exact catalog publication attestation subjects another object");
    }
    let publication = BundleCatalogPublication::from_current_value(&read_exact(
        objects,
        &coordinate.catalog_publication_hash,
    )?)?;
    if publication.catalog_namespace != catalog_namespace
        || publication.snapshot_hash != coordinate.catalog_snapshot_hash
    {
        bail!("exact catalog coordinate disagrees with publication");
    }
    let snapshot = BundleCatalogSnapshot::from_current_value(&read_exact(
        objects,
        &coordinate.catalog_snapshot_hash,
    )?)?;
    if snapshot.publisher != publication_attestation.issuer
        || !snapshot
            .set_channels
            .iter()
            .any(|entry| entry.set_attestation_hash == coordinate.set_attestation_hash)
    {
        bail!("exact curated set is not retained by the signed catalog snapshot");
    }
    let set_attestation = read_attestation(objects, &coordinate.set_attestation_hash)?;
    verify_publisher_attestation(policy, &set_attestation, BUNDLE_SET_RELEASE_CLAIM)?;
    if set_attestation.issuer != snapshot.publisher
        || set_attestation.subject_hash != coordinate.set_hash
    {
        bail!("exact curated-set coordinate disagrees with publisher attestation");
    }
    Ok(())
}

pub fn verify_curated_set(
    set_attestation_hash: &str,
    objects: &impl PublicationObjectReader,
    policy: &(impl ConsumerPublicationPolicy + ?Sized),
    materialization_proof: &(impl PublisherMaterializationProof + ?Sized),
    release_evidence_proof: &(impl BundleReleaseEvidenceProof + ?Sized),
    policy_binding: &ReleasePolicyBinding,
) -> anyhow::Result<VerifiedConsumerSet> {
    let set_attestation = read_attestation(objects, set_attestation_hash)?;
    verify_publisher_attestation(policy, &set_attestation, BUNDLE_SET_RELEASE_CLAIM)?;
    let set_hash = set_attestation.subject_hash.clone();
    let set = BundleSet::from_current_value(&read_exact(objects, &set_hash)?)?;
    verify_prospective_set(
        set_hash,
        set_attestation_hash.to_owned(),
        set,
        Some(&set_attestation.issuer),
        objects,
        policy,
        materialization_proof,
        release_evidence_proof,
        policy_binding,
    )
}

/// Admit a complete prospective set before it is retained or signed. Every
/// generation authorization and all generation evidence are verified exactly;
/// callers cannot turn structural set decoding into publication authority.
pub fn verify_prospective_set(
    set_hash: String,
    set_attestation_hash: String,
    set: BundleSet,
    expected_publisher: Option<&str>,
    objects: &impl PublicationObjectReader,
    policy: &(impl ConsumerPublicationPolicy + ?Sized),
    materialization_proof: &(impl PublisherMaterializationProof + ?Sized),
    release_evidence_proof: &(impl BundleReleaseEvidenceProof + ?Sized),
    policy_binding: &ReleasePolicyBinding,
) -> anyhow::Result<VerifiedConsumerSet> {
    set.validate()?;
    if set.migration_requirement != MigrationRequirement::None {
        bail!("consumer v1 refuses a set requiring migration");
    }
    let mut generations = BTreeMap::new();
    for entry in &set.entries {
        let attestation = read_attestation(objects, &entry.publisher_attestation_hash)?;
        verify_publisher_attestation(policy, &attestation, BUNDLE_GENERATION_RELEASE_CLAIM)?;
        if expected_publisher.is_some_and(|publisher| attestation.issuer != publisher) {
            bail!("generation and prospective set publishers disagree");
        }
        if attestation.subject_hash != entry.generation_hash {
            bail!("generation attestation subjects a different generation");
        }
        let generation =
            BundleGeneration::from_current_value(&read_exact(objects, &entry.generation_hash)?)?;
        if generation.bundle_name != entry.bundle_name
            || !generation_target_fits_set(&generation.target, &set.target)
            || generation.substrate_protocol != set.substrate_protocol
        {
            bail!("bundle set entry and generation identity disagree");
        }
        let verified = verify_bundle_generation(
            generation,
            objects,
            materialization_proof,
            release_evidence_proof,
            policy_binding,
        )?;
        generations.insert(entry.bundle_name.clone(), verified);
    }
    if !generations.contains_key(CORE_BUNDLE_NAME) {
        bail!("complete bundle set omits core");
    }
    let substrate_release_attestation =
        read_attestation(objects, &set.substrate_release_attestation_hash)?;
    verify_publisher_attestation(
        policy,
        &substrate_release_attestation,
        SUBSTRATE_RELEASE_CLAIM,
    )?;
    if expected_publisher.is_some_and(|publisher| substrate_release_attestation.issuer != publisher)
    {
        bail!("substrate release and prospective set publishers disagree");
    }
    let substrate_release = SubstrateRelease::from_current_value(&read_exact(
        objects,
        &substrate_release_attestation.subject_hash,
    )?)?;
    if substrate_release.catalog_namespace != policy_binding.catalog_namespace
        || substrate_release.bundle_publication_policy_section_digest
            != policy_binding.bundle_publication_policy_section_digest
        || substrate_release.trust_epoch != policy_binding.trust_epoch
    {
        bail!("substrate release differs from current publication policy binding");
    }
    let substrate_build =
        ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult::from_value(
            &read_exact(objects, &substrate_release.substrate_build_accepted_result_hash)?,
        )?;
    let selected_substrate = substrate_build
        .products
        .iter()
        .find(|product| {
            product.product_name == substrate_release.selected_substrate_product_identity
        })
        .context("substrate release selected product is absent from accepted result")?;
    if selected_substrate.witness_hash != substrate_release.selected_substrate_product_witness
        || substrate_release.qualification_evidence_hashes.len() != 1
    {
        bail!("substrate release build and qualification evidence disagree");
    }
    release_evidence_proof
        .verify_substrate_release_evidence(&substrate_release, &substrate_build, policy_binding)
        .context("substrate release evidence is unproven")?;
    verify_substrate_release_binding(&set, &substrate_release)?;
    Ok(VerifiedConsumerSet {
        set_hash,
        set_attestation_hash,
        set,
        substrate_release,
        generations,
    })
}

fn verify_substrate_release_binding(
    set: &BundleSet,
    substrate_release: &SubstrateRelease,
) -> anyhow::Result<()> {
    let core = set
        .entries
        .iter()
        .find(|entry| entry.bundle_name == CORE_BUNDLE_NAME)
        .context("complete bundle set omits core")?;
    if substrate_release.core_generation_hash != core.generation_hash
        || substrate_release.core_generation_attestation_hash != core.publisher_attestation_hash
        || substrate_release.substrate_protocol != set.substrate_protocol
        || substrate_release.target != set.target
    {
        bail!("substrate release does not authorize this set's exact Core and substrate");
    }
    Ok(())
}

pub fn plan_fetch(set: &VerifiedConsumerSet) -> ConsumerFetchPlan {
    let requests = set
        .set
        .entries
        .iter()
        .map(|entry| {
            let generation = set.generations[&entry.bundle_name].generation();
            BundleFetchRequest {
                bundle_name: entry.bundle_name.clone(),
                generation_hash: entry.generation_hash.clone(),
                generation_attestation_hash: entry.publisher_attestation_hash.clone(),
                // Core is seeded by and verified against the substrate. Its payload
                // is never fetched by normal bundle publication.
                content_manifest_hash: (entry.bundle_name != CORE_BUNDLE_NAME)
                    .then(|| generation.content_manifest_hash.clone()),
            }
        })
        .collect();
    ConsumerFetchPlan {
        set_attestation_hash: set.set_attestation_hash.clone(),
        set_hash: set.set_hash.clone(),
        requests,
    }
}

pub fn plan_exact_set(
    set: &VerifiedConsumerSet,
    installed: &BTreeMap<String, String>,
) -> anyhow::Result<ExactSetPlan> {
    let selected: BTreeMap<_, _> = set
        .set
        .entries
        .iter()
        .map(|entry| (entry.bundle_name.clone(), entry.generation_hash.clone()))
        .collect();
    let names: BTreeSet<_> = installed.keys().chain(selected.keys()).cloned().collect();
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        let before = installed.get(&name).cloned();
        let after = selected.get(&name).cloned();
        let action = match (&before, &after) {
            (None, Some(_)) => ExactSetAction::Add,
            (Some(_), None) => ExactSetAction::Remove,
            (Some(left), Some(right)) if left == right => ExactSetAction::Keep,
            (Some(_), Some(_)) => ExactSetAction::Replace,
            (None, None) => unreachable!(),
        };
        if name == CORE_BUNDLE_NAME && action != ExactSetAction::Keep {
            bail!("normal bundle reconciliation requires core to remain exact and keep-only");
        }
        entries.push(ExactSetPlanEntry {
            bundle_name: name,
            action,
            installed_generation_hash: before,
            selected_generation_hash: after,
        });
    }
    Ok(ExactSetPlan {
        set_attestation_hash: set.set_attestation_hash.clone(),
        set_hash: set.set_hash.clone(),
        entries,
        requires_stopped_node: true,
    })
}

pub fn admit_node_bundle_selection(
    authorization_hash: &str,
    verified_set: &VerifiedConsumerSet,
    context: SelectionAdmissionContext<'_>,
    objects: &impl PublicationObjectReader,
    policy: &(impl ConsumerPublicationPolicy + ?Sized),
) -> anyhow::Result<AdmittedNodeBundleSelection> {
    let authorization = read_attestation(objects, authorization_hash)?;
    policy.verify_deployment_attestation(&authorization)?;
    if authorization.claim != DEPLOYMENT_SELECTION_CLAIM
        || authorization.policy != BUNDLE_DEPLOYMENT_POLICY
        || authorization.expires_at.is_some()
    {
        bail!("deployment authorization has the wrong closed claim, policy, or expiry");
    }
    let selection_hash = authorization.subject_hash.clone();
    let selection =
        NodeBundleSelection::from_current_value(&read_exact(objects, &selection_hash)?)?;
    if selection.target_node_or_app_root_identity != context.target_identity
        || selection.substrate_image_digest != context.substrate_image_digest
        || selection.substrate_protocol != context.substrate_protocol
        || selection.bundle_set_hash != verified_set.set_hash
        || selection.curated_set_attestation_hash.as_deref()
            != Some(verified_set.set_attestation_hash.as_str())
        || selection.bundle_publication_policy_section_digest
            != context.publication_policy_section_digest
        || selection.node_policy_generation_digest != context.node_policy_generation_digest
        || selection.expected_active_selection.as_deref() != context.active_selection
        || selection.migration_decision != MigrationDecision::None
    {
        bail!("node bundle selection does not exactly match consumer admission context");
    }
    if verified_set.set.substrate_protocol != context.substrate_protocol {
        bail!("selected bundle set and substrate protocol disagree");
    }
    if &verified_set.set.target != context.bundle_target {
        bail!("selected bundle set and consumer target disagree");
    }
    if verified_set.substrate_release.substrate_image_digest != context.substrate_image_digest
        || verified_set.substrate_release.substrate_protocol != context.substrate_protocol
        || &verified_set.substrate_release.target != context.bundle_target
    {
        bail!("selected bundle set belongs to a different substrate release");
    }
    Ok(AdmittedNodeBundleSelection {
        selection_hash,
        authorization_hash: authorization_hash.into(),
        selection,
    })
}

// Portable generations contain no platform payload and can participate in a
// native set. Native generations must match the set exactly, including ABI;
// a portable set must never conceal a native generation.
fn generation_target_fits_set(generation: &BundleTarget, set: &BundleTarget) -> bool {
    matches!(generation, BundleTarget::Portable) || generation == set
}

fn read_attestation(
    objects: &impl PublicationObjectReader,
    hash: &str,
) -> anyhow::Result<Attestation> {
    Attestation::from_value(&read_exact(objects, hash)?)
        .with_context(|| format!("bundle publication attestation {hash} is invalid"))
}

fn verify_publisher_attestation(
    policy: &(impl ConsumerPublicationPolicy + ?Sized),
    attestation: &Attestation,
    expected_claim: &str,
) -> anyhow::Result<()> {
    if attestation.claim != expected_claim
        || attestation.policy != BUNDLE_PUBLICATION_POLICY
        || attestation.expires_at.is_some()
    {
        bail!("publisher attestation has the wrong closed claim, policy, or expiry");
    }
    policy.verify_publisher_attestation(attestation, expected_claim)
}

fn read_exact(
    objects: &impl PublicationObjectReader,
    expected_hash: &str,
) -> anyhow::Result<Value> {
    let value = objects
        .get_object(expected_hash)?
        .with_context(|| format!("bundle publication object {expected_hash} is absent"))?;
    let actual = lillux::cas::sha256_hex(lillux::canonical_json(&value)?.as_bytes());
    if actual != expected_hash {
        bail!("bundle publication object content disagrees with requested CAS identity");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_bundle_publication_contract::{BUNDLE_SET_KIND, BUNDLE_SET_SCHEMA, BundleSetEntry};

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    #[test]
    fn portable_members_compose_into_native_sets_without_erasing_native_target() {
        let portable = BundleTarget::Portable;
        let gnu = BundleTarget::Triple {
            triple: "x86_64-unknown-linux-gnu".into(),
        };
        let musl = BundleTarget::Triple {
            triple: "x86_64-unknown-linux-musl".into(),
        };
        let arm = BundleTarget::Triple {
            triple: "aarch64-unknown-linux-gnu".into(),
        };
        assert!(generation_target_fits_set(&portable, &portable));
        assert!(generation_target_fits_set(&portable, &gnu));
        assert!(generation_target_fits_set(&gnu, &gnu));
        assert!(!generation_target_fits_set(&gnu, &portable));
        assert!(!generation_target_fits_set(&gnu, &musl));
        assert!(!generation_target_fits_set(&gnu, &arm));
    }

    fn verified_set(entries: &[(&str, char)]) -> VerifiedConsumerSet {
        let entries = entries
            .iter()
            .map(|(name, byte)| BundleSetEntry {
                bundle_name: (*name).into(),
                generation_hash: hash(*byte),
                publisher_attestation_hash: hash(byte.to_ascii_uppercase()),
            })
            .collect();
        VerifiedConsumerSet {
            set_hash: hash('f'),
            set_attestation_hash: hash('e'),
            set: BundleSet {
                schema: BUNDLE_SET_SCHEMA.into(),
                kind: BUNDLE_SET_KIND.into(),
                set_name: "standard".into(),
                target: BundleTarget::Portable,
                substrate_protocol: 1,
                substrate_release_attestation_hash: hash('d'),
                entries,
                migration_requirement: MigrationRequirement::None,
            },
            substrate_release: SubstrateRelease {
                schema: ryeos_bundle_publication_contract::SUBSTRATE_RELEASE_SCHEMA.into(),
                kind: ryeos_bundle_publication_contract::SUBSTRATE_RELEASE_KIND.into(),
                catalog_namespace: "official".into(),
                bundle_publication_policy_section_digest: hash('8'),
                trust_epoch: 1,
                substrate_image_digest: format!("sha256:{}", hash('c')),
                substrate_protocol: 1,
                target: BundleTarget::Portable,
                substrate_build_accepted_result_hash: hash('9'),
                substrate_build_receipt_hash: hash('7'),
                selected_substrate_product_identity: "substrate".into(),
                selected_substrate_product_witness: hash('a'),
                qualification_evidence_hashes: vec![hash('b')],
                core_generation_hash: hash('3'),
                core_generation_attestation_hash: hash('3'.to_ascii_uppercase()),
            },
            generations: BTreeMap::new(),
        }
    }

    #[test]
    fn exact_plan_reports_all_four_actions_without_mutation() {
        let set = verified_set(&[("added", '1'), ("changed", '2'), ("core", '3')]);
        let installed = BTreeMap::from([
            ("changed".into(), hash('4')),
            ("core".into(), hash('3')),
            ("removed".into(), hash('5')),
        ]);
        let plan = plan_exact_set(&set, &installed).unwrap();
        let actions: BTreeMap<_, _> = plan
            .entries
            .iter()
            .map(|entry| (entry.bundle_name.as_str(), entry.action))
            .collect();
        assert_eq!(actions["added"], ExactSetAction::Add);
        assert_eq!(actions["changed"], ExactSetAction::Replace);
        assert_eq!(actions["core"], ExactSetAction::Keep);
        assert_eq!(actions["removed"], ExactSetAction::Remove);
        assert!(plan.requires_stopped_node);
    }

    #[test]
    fn exact_plan_refuses_core_replacement_or_removal() {
        let replacement = verified_set(&[("core", '2')]);
        let installed = BTreeMap::from([("core".into(), hash('1'))]);
        assert!(
            plan_exact_set(&replacement, &installed)
                .unwrap_err()
                .to_string()
                .contains("core")
        );

        let omission = verified_set(&[("standard", '2')]);
        assert!(
            plan_exact_set(&omission, &installed)
                .unwrap_err()
                .to_string()
                .contains("core")
        );
    }

    #[test]
    fn substrate_release_must_bind_exact_core_protocol_and_target() {
        let set = verified_set(&[("core", '3'), ("standard", '4')]);
        assert!(verify_substrate_release_binding(&set.set, &set.substrate_release).is_ok());

        let mut wrong_core = set.substrate_release.clone();
        wrong_core.core_generation_hash = hash('9');
        assert!(verify_substrate_release_binding(&set.set, &wrong_core).is_err());

        let mut wrong_protocol = set.substrate_release.clone();
        wrong_protocol.substrate_protocol = 2;
        assert!(verify_substrate_release_binding(&set.set, &wrong_protocol).is_err());

        let mut wrong_target = set.substrate_release.clone();
        wrong_target.target = BundleTarget::Triple {
            triple: "x86_64-unknown-linux-gnu".into(),
        };
        assert!(verify_substrate_release_binding(&set.set, &wrong_target).is_err());
    }
}
