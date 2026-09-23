//! Operator-selected build products admitted through ordinary content bindings.
//!
//! Signed source supplies the finite relationship. Invocation selectors choose
//! exact node testimony, never authority, a filesystem path, or an output digest.
//! Selection preserves the admitted execution owner across local and configured
//! remote ingress. Node-local precomposition cannot substitute another owner.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_engine::contracts::{ItemSpace, SubjectResolutionAuthority};
use ryeos_engine::external_content::{
    authored_external_content_shape, declaring_authority, insert_resolved_product_selections,
    pre_product_selection_consumer_digest, resolved_external_product_selections,
};
use ryeos_engine::resolution::ResolutionOutput;
use ryeos_state::external_content::products::composition::{
    MAX_PRODUCT_SELECTION_INPUTS_BYTES, MAX_PRODUCT_SELECTIONS, ProductRelationships,
    ProductSelection, ResolvedExternalProductSelection, ResolvedExternalProductSelections,
    ResolvedProductConsumerSource, ResolvedProductDeclaration,
};
use ryeos_state::external_content::products::publication::ProductCaptureCoordinate;
use serde::{Deserialize, Serialize};

use super::{BindResponse, RetainedProductImportRequest};
use crate::handler_context::HandlerContext;
use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::state::AppState;

/// Root selector admission uses the already-admitted execution principal.
/// This does not import content or create bindings: explicit composition must
/// have completed before the ordinary realization owner can admit the launch.
pub fn admit_root_product_selections(
    state: &AppState,
    current_site_id: &str,
    engine: &ryeos_engine::engine::Engine,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    subject: &SubjectResolutionAuthority,
    resolution: &mut ResolutionOutput,
    owner: Option<&str>,
    context: Option<&HandlerContext>,
    inputs: &ryeos_state::external_content::products::composition::ProductSelectionInputs,
    recovered: bool,
) -> anyhow::Result<()> {
    use ryeos_state::external_content::products::composition::ProductSelectionTarget;
    // Run before filtering root slots: a content-dependency-only selection is
    // also local to this serving node. Origin is not consulted: it identifies
    // the authenticated caller's provenance, not where these products reside.
    // Fresh and recovered callers pass the current site from admitted authority.
    if (!inputs.is_empty() || resolved_external_product_selections(resolution)?.is_some())
        && current_site_id != state.threads.site_id()
    {
        bail!("product-selected execution current site differs from the serving node");
    }
    let selectors = inputs
        .iter()
        .filter_map(|input| match &input.target {
            ProductSelectionTarget::Root {} => Some(input.selection.clone()),
            ProductSelectionTarget::ContentDependency { .. }
            | ProductSelectionTarget::WorkloadExecution { .. } => None,
        })
        .collect::<Vec<_>>();
    if selectors.is_empty() && resolved_external_product_selections(resolution)?.is_none() {
        return Ok(());
    }
    let owner =
        owner.ok_or_else(|| anyhow::anyhow!("root product selection has no admitted owner"))?;
    if recovered {
        verify_recovered_selections(state, resolution, subject, owner, &selectors)?;
        let kind =
            ryeos_engine::canonical_ref::CanonicalRef::parse(&resolution.root.resolved_ref)?.kind;
        let contract = engine
            .kinds
            .get(&kind)
            .and_then(|schema| schema.external_content_contract());
        let mut before_realization = resolution.clone();
        before_realization
            .composed
            .derived
            .remove(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY);
        ryeos_engine::external_content::effective_external_content_declarations(
            &before_realization,
            contract,
            declaring_authority(&before_realization)?,
        )?
        .context("retained root selections have no signed declaration")?;
        let realized = resolution
            .composed
            .derived
            .get(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
            .map(ryeos_engine::external_realization::RealizedExternalContentSet::from_value)
            .transpose()?
            .context("retained root selections have no admitted realizations")?;
        let selections = resolved_external_product_selections(resolution)?
            .context("retained root selection projection is absent")?;
        for (_, selected) in selections.iter() {
            if !realized.iter().any(|entry| {
                entry.id == selected.declaration_id
                    && entry.kind == selected.declaration.kind
                    && entry.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
                    && entry.manifest_hash == selected.manifest_hash
                    && entry.mount_root == selected.declaration.mount_root
                    && entry.mount == selected.declaration.mount
            }) {
                bail!("retained root product realization contradicts its exact selected slot");
            }
        }
        Ok(())
    } else {
        let context = context.context("root product selection has no authenticated ingress")?;
        if context.fingerprint != owner {
            bail!("root product selection ingress differs from its admitted owner");
        }
        select_products(
            state, context, engine, roots, subject, resolution, &selectors,
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeRetainedProductsRequest {
    pub consumer_ref: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub project_context: Option<ProductCompositionProjectContext>,
    pub selections: Vec<ProductSelection>,
    pub maximum_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductCompositionProjectContext {
    pub snapshot_hash: String,
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl ComposeRetainedProductsRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_selection_batch(&self.selections)?;
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(&self.consumer_ref)?;
        if canonical.to_string() != self.consumer_ref || canonical.suffix.is_some() {
            bail!("product composition requires an exact canonical consumer ref");
        }
        if let Some(project) = &self.project_context {
            if !lillux::valid_hash(&project.snapshot_hash)
                || project
                    .snapshot_hash
                    .bytes()
                    .any(|b| b.is_ascii_uppercase())
            {
                bail!("product composition context requires an exact canonical snapshot");
            }
        }
        if self.maximum_bytes == 0 {
            bail!("product composition requires a positive aggregate import bound");
        }
        Ok(())
    }

    pub fn subject_resolution_authority(&self) -> SubjectResolutionAuthority {
        match &self.project_context {
            Some(project) => SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: project.snapshot_hash.clone(),
            },
            None => SubjectResolutionAuthority::Projectless,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeRetainedProductsResponse {
    pub consumer: ryeos_state::objects::ExternalContentConsumerAuthority,
    pub project_context: Option<ProductCompositionProjectContext>,
    pub pre_selection_effective_definition_digest: String,
    pub selected_effective_definition_digest: String,
    pub selections: Vec<ProductSelection>,
    pub bindings: Vec<ProductCompositionBinding>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductCompositionBinding {
    pub declaration_ids: Vec<String>,
    pub manifest_kind: String,
    pub manifest_hash: String,
    pub total_bytes: u64,
    pub binding: BindResponse,
}

pub(super) fn validate_selection_batch(selectors: &[ProductSelection]) -> anyhow::Result<()> {
    if selectors.is_empty()
        || selectors.len() > MAX_PRODUCT_SELECTIONS
        || lillux::canonical_json(&serde_json::to_value(selectors)?)?.len()
            > MAX_PRODUCT_SELECTION_INPUTS_BYTES
    {
        bail!("product selections exceed the bounded nonempty batch contract");
    }
    for selector in selectors {
        selector.validate()?;
    }
    if selectors
        .windows(2)
        .any(|pair| pair[0].declaration_id >= pair[1].declaration_id)
    {
        bail!("product selections must have unique declaration IDs in canonical order");
    }
    Ok(())
}

/// Verify a complete selection against the exact request engine's source view.
/// This is synchronous CPU/CAS work; operator API calls run it on a blocking
/// worker. It grants no import or binding authority during launch preparation.
#[allow(clippy::too_many_arguments)]
pub fn select_products(
    state: &AppState,
    context: &HandlerContext,
    engine: &ryeos_engine::engine::Engine,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    subject: &SubjectResolutionAuthority,
    resolution: &mut ResolutionOutput,
    selectors: &[ProductSelection],
) -> anyhow::Result<ResolvedExternalProductSelections> {
    select_products_verified(
        state, context, engine, roots, subject, resolution, selectors,
    )
    .map(|(selections, _)| selections)
}

#[allow(clippy::too_many_arguments)]
fn select_products_verified(
    state: &AppState,
    context: &HandlerContext,
    engine: &ryeos_engine::engine::Engine,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    subject: &SubjectResolutionAuthority,
    resolution: &mut ResolutionOutput,
    selectors: &[ProductSelection],
) -> anyhow::Result<(
    ResolvedExternalProductSelections,
    BTreeMap<String, ryeos_state::external_content::products::publication::VerifiedProductWitness>,
)> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    validate_selection_batch(selectors)?;
    let canonical =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&resolution.root.resolved_ref)?;
    let contract = engine
        .kinds
        .get(&canonical.kind)
        .and_then(|kind| kind.external_content_contract())
        .context("product consumer has no signed external-content contract")?;
    let shape = authored_external_content_shape(
        &resolution.composed.composed,
        Some(contract),
        declaring_authority(resolution)?,
    )?
    .context("product consumer has no signed content declarations")?;
    if shape.product_slots.len() != selectors.len()
        || shape.product_slots.iter().any(|slot| {
            !selectors
                .iter()
                .any(|selector| selector.declaration_id == slot.id)
        })
    {
        bail!("product selections do not exactly cover the signed consumer slots");
    }
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let mut proofs = BTreeMap::new();
    let mut witnesses = BTreeMap::new();
    for selector in selectors {
        let witness = super::product_receipt::load_product_source(
            state,
            &authority,
            &guard,
            limits,
            &context.fingerprint,
            &selector.witness_hash,
            &selector.witness_source,
            super::product_receipt::ProductSourceVerification::Fresh,
        )?;
        let selection = select_verified_product(
            state, context, engine, roots, subject, resolution, selector, &authority, &guard,
            &witness, limits,
        )?;
        proofs.insert(selector.declaration_id.clone(), selection);
        witnesses.insert(selector.declaration_id.clone(), witness);
    }
    let proofs = ResolvedExternalProductSelections::new(proofs)?;
    insert_resolved_product_selections(resolution, proofs.clone(), contract)?;
    Ok((proofs, witnesses))
}

/// Prove the entire batch and aggregate bound before staging any manifest.
/// Each ordinary import authenticates the exact selected witness again while
/// holding its publication guard; binding uses the already checked full D1.
pub fn select_and_import_products(
    state: Arc<AppState>,
    context: HandlerContext,
    request: &ComposeRetainedProductsRequest,
    engine: &ryeos_engine::engine::Engine,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    resolution: &mut ResolutionOutput,
) -> anyhow::Result<PreparedProductImports> {
    request.validate()?;
    if resolution.root.resolved_ref != request.consumer_ref {
        bail!("product composition resolution disagrees with its requested consumer");
    }
    let subject = request.subject_resolution_authority();
    let d0 = pre_product_selection_consumer_digest(resolution)?;
    let (proofs, witnesses) = select_products_verified(
        &state,
        &context,
        engine,
        roots,
        &subject,
        resolution,
        &request.selections,
    )?;
    let import_policy = state.node_policy.require::<crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord>()?;
    if request.maximum_bytes > import_policy.limits.max_total_bytes {
        bail!("product batch import ceiling exceeds node policy");
    }
    let manifests = manifest_groups(&witnesses, request.maximum_bytes)?;
    let mut imports = Vec::with_capacity(manifests.len());
    for ((kind, hash), (declaration_ids, total_bytes)) in manifests {
        let selector = request
            .selections
            .iter()
            .find(|selector| selector.declaration_id == declaration_ids[0])
            .context("prepared product manifest lost its selector")?;
        let expected = proofs
            .get(&selector.declaration_id)
            .context("prepared product proof disappeared")?;
        let (imported, ()) = super::retained_product::import_with_verified(
            Arc::clone(&state),
            context.clone(),
            RetainedProductImportRequest {
                witness_hash: selector.witness_hash.clone(),
                witness_source: selector.witness_source.clone(),
                maximum_bytes: request.maximum_bytes,
            },
            |_, _, witness, _| {
                verify_witness_projection(expected, witness)?;
                Ok(())
            },
        )?;
        if imported.manifest_kind != kind
            || imported.manifest_hash != hash
            || imported.total_bytes != total_bytes
        {
            bail!("fresh product import contradicts the verified batch");
        }
        imports.push((declaration_ids, imported));
    }
    Ok(PreparedProductImports {
        request_digest: lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(request)?)?.as_bytes(),
        ),
        d0,
        selections: proofs,
        imports,
    })
}

/// Private fields prevent caller-authored imports or projections from becoming
/// a prepared batch. Ordinary durable stages retain bytes between preparation
/// and the existing per-manifest binding transactions.
pub struct PreparedProductImports {
    request_digest: String,
    d0: String,
    selections: ResolvedExternalProductSelections,
    imports: Vec<(Vec<String>, super::ImportResponse)>,
}

/// Relationship lookup uses the already-admitted source generation even when
/// execution has a writable COW realization. It does not change the consumer's
/// Bundle/Project source identity or promote a live path into pinned authority.
fn relationship_resolution_request(
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    subject: &SubjectResolutionAuthority,
    relationship_ref: &str,
) -> anyhow::Result<ryeos_engine::engine::EffectiveItemRequest> {
    let project_root = match subject {
        SubjectResolutionAuthority::PinnedGeneration { .. }
        | SubjectResolutionAuthority::CowWorkspace { .. } => Some(
            roots
                .authoritative_project_root()?
                .context("product selection lost its admitted pinned context")?
                .to_path_buf(),
        ),
        SubjectResolutionAuthority::Projectless => None,
        SubjectResolutionAuthority::LiveFs => {
            bail!("product composition requires pinned or explicitly projectless resolution")
        }
    };
    Ok(ryeos_engine::engine::EffectiveItemRequest {
        item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(relationship_ref)?,
        expected_kind: Some("config".to_owned()),
        project_root,
        subject_resolution_authority: subject.clone(),
    })
}

#[allow(clippy::too_many_arguments)]
fn select_verified_product(
    state: &AppState,
    context: &HandlerContext,
    engine: &ryeos_engine::engine::Engine,
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    subject: &SubjectResolutionAuthority,
    resolution: &ResolutionOutput,
    selector: &ProductSelection,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    witness: &ryeos_state::external_content::products::publication::VerifiedProductWitness,
    closure_limits: ryeos_state::object_closure::ObjectClosureLimits,
) -> anyhow::Result<ResolvedExternalProductSelection> {
    selector.validate()?;
    authority.ensure_guard(guard)?;
    if witness.attestation_hash != selector.witness_hash
        || witness.evidence.owner_principal != context.fingerprint
    {
        bail!("verified product does not match selected witness and owner");
    }
    let consumer_source = product_consumer_source(resolution, subject)?;
    let canonical =
        ryeos_engine::canonical_ref::CanonicalRef::parse(&resolution.root.resolved_ref)?;
    let contract = engine
        .kinds
        .get(&canonical.kind)
        .and_then(|kind| kind.external_content_contract())
        .context("product consumer has no signed external-content contract")?;
    let shape = authored_external_content_shape(
        &resolution.composed.composed,
        Some(contract),
        declaring_authority(resolution)?,
    )?
    .context("product consumer has no content declarations")?;
    let slot = shape
        .product_slots
        .iter()
        .find(|slot| slot.id == selector.declaration_id)
        .context("product selection does not name an authored consumer slot")?;
    let base_digest = pre_product_selection_consumer_digest(resolution)?;
    let relationship_resolution = engine.effective_resolution_output(
        relationship_resolution_request(roots, subject, &slot.relationship_ref)?,
    )?;
    if relationship_resolution.root.signer_fingerprint.is_none()
        || !matches!(
            relationship_resolution.effective_trust_class,
            ryeos_engine::resolution::TrustClass::TrustedProject
                | ryeos_engine::resolution::TrustClass::TrustedBundle
        )
        || !matches!(
            relationship_resolution.root.source_space,
            ItemSpace::Project | ItemSpace::Bundle
        )
    {
        bail!("product relationship requires currently trusted signed project or bundle source");
    }
    let relationships = ProductRelationships::from_value(
        relationship_resolution
            .composed
            .composed
            .get("product_relationships")
            .cloned()
            .context("signed relationship Config has no product_relationships")?,
    )?;
    let relationship = relationships
        .relationships
        .iter()
        .find(|value| value.name == slot.relationship)
        .context("signed product relationship is absent")?
        .clone();
    if relationship.consumer.canonical_ref != resolution.root.resolved_ref
        || relationship.consumer.declaration_id != selector.declaration_id
    {
        bail!("signed product relationship names a different consumer or slot");
    }
    let evidence = &witness.evidence;
    if relationship_resolution.root.resolved_ref != slot.relationship_ref {
        bail!("resolved product relationship disagrees with the exact signed consumer slot");
    }
    relationship.validate_compatible_product_evidence(evidence)?;
    let qualification = admit_selected_qualification(
        state,
        context,
        authority,
        guard,
        closure_limits,
        selector,
        &relationship,
        witness,
    )?;
    ryeos_state::external_content::products::publication::verify_product_manifest_against_bounds(
        authority,
        evidence,
        &relationship.required_product.bounds,
        closure_limits,
        guard,
    )?;
    let selection = ResolvedExternalProductSelection {
        schema: ryeos_state::external_content::products::composition::RESOLVED_EXTERNAL_PRODUCT_SELECTION_SCHEMA.to_owned(),
        declaration_id: slot.id.clone(),
        relationship_name: slot.relationship.clone(),
        relationship_ref: slot.relationship_ref.clone(),
        relationship_raw_content_digest: relationship_resolution.root.raw_content_digest.clone(),
        relationship,
        witness_hash: selector.witness_hash.clone(),
        witness_source: selector.witness_source.clone(),
        witness_coordinate: ProductCaptureCoordinate::from_evidence(evidence)?,
        qualification,
        producer: evidence.root_producer.clone(),
        owner_principal: evidence.owner_principal.clone(),
        consumer_source,
        pre_selection_effective_definition_digest: base_digest,
        manifest_hash: evidence.manifest_hash.clone(),
        manifest_kind: evidence.manifest_kind.clone(),
        declaration: ResolvedProductDeclaration {
            id: slot.id.clone(), kind: slot.kind, manifest_hash: evidence.manifest_hash.clone(),
            mount_root: slot.mount_root, mount: slot.mount.clone(),
        },
    };
    selection.validate()?;
    Ok(selection)
}

#[allow(clippy::too_many_arguments)]
fn admit_selected_qualification(
    state: &AppState,
    context: &HandlerContext,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    selector: &ProductSelection,
    relationship: &ryeos_state::external_content::products::composition::ProductRelationship,
    product: &ryeos_state::external_content::products::publication::VerifiedProductWitness,
) -> anyhow::Result<
    Option<ryeos_state::external_content::products::composition::AdmittedProductQualification>,
> {
    use super::product_qualification as qualification;
    use ryeos_state::external_content::products::composition::AdmittedProductQualification;
    let (policy_ref, hash) = match (
        &relationship.qualification.policy_ref,
        &selector.qualification_hash,
    ) {
        (None, None) => return Ok(None),
        (Some(policy), Some(hash)) => (policy, hash),
        _ => bail!("product selection must provide exactly its declared qualification proof"),
    };
    let proof = qualification::load_current_qualification(
        state,
        authority,
        guard,
        limits,
        &context.fingerprint,
        hash,
    )?;
    if proof.evidence.product_witness_hash != product.attestation_hash
        || proof.evidence.product_coordinate
            != ProductCaptureCoordinate::from_evidence(&product.evidence)?
        || proof.evidence.result.subject_manifest_hash != product.evidence.manifest_hash
    {
        bail!("qualification does not prove the exact selected product witness and manifest");
    }
    let current_policy =
        qualification::resolve_current_bundle_qualification_policy(state, policy_ref)?;
    let current_verifier = qualification::resolve_current_bundle_verifier_identity_for_evidence(
        state,
        authority,
        guard,
        limits,
        context,
        &current_policy.policy.verifier_ref,
        &current_policy.policy.verifier_parameters,
        &proof.evidence,
    )?;
    proof.evidence.validate_current_policy(
        &current_policy,
        &current_verifier.effective_definition_digest,
        &relationship.qualification.required_claims,
    )?;
    proof
        .evidence
        .validate_current_artifact(&current_verifier.artifact_identity)?;
    qualification::execution_evidence::verify_current(
        state,
        authority,
        guard,
        context,
        &proof.evidence,
        &current_verifier,
    )?;
    Ok(Some(AdmittedProductQualification {
        attestation_hash: proof.attestation_hash,
        evidence: proof.evidence,
    }))
}

/// A recovered capsule has already admitted source, relationship and testimony.
/// Validate those retained facts against the sealed selector; do not replace
/// them with today's source, a newer witness head, or a mutable project path.
pub fn verify_recovered_selections(
    state: &AppState,
    resolution: &ResolutionOutput,
    subject: &SubjectResolutionAuthority,
    owner_principal: &str,
    selectors: &[ProductSelection],
) -> anyhow::Result<()> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    verify_recovered_selections_guarded(
        state,
        &authority,
        &guard,
        limits,
        resolution,
        subject,
        owner_principal,
        selectors,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn verify_recovered_selections_guarded(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    resolution: &ResolutionOutput,
    subject: &SubjectResolutionAuthority,
    owner_principal: &str,
    selectors: &[ProductSelection],
) -> anyhow::Result<()> {
    authority.ensure_guard(guard)?;
    validate_selection_batch(selectors)?;
    let source = product_consumer_source(resolution, subject)?;
    let selections = resolved_external_product_selections(resolution)?
        .context("recovered product consumer lost its admitted selections")?;
    if selections.len() != selectors.len() {
        bail!("recovered product consumer has a different selection count");
    }
    for selector in selectors {
        let selection = selections
            .get(&selector.declaration_id)
            .context("recovered product consumer lost the sealed selection")?;
        selection.validate()?;
        if let Some(proof) = &selection.qualification {
            super::product_qualification::verify_retained_qualification_guarded(
                state,
                authority,
                guard,
                limits,
                proof,
                owner_principal,
            )?;
        }
        if selection.witness_hash != selector.witness_hash
            || selection.witness_source != selector.witness_source
            || selection
                .qualification
                .as_ref()
                .map(|proof| &proof.attestation_hash)
                != selector.qualification_hash.as_ref()
            || selection.owner_principal != owner_principal
            || selection.consumer_source != source
        {
            bail!("recovered product selection contradicts sealed invocation authority");
        }
        let witness = super::product_receipt::load_product_source(
            state,
            authority,
            guard,
            limits,
            owner_principal,
            &selection.witness_hash,
            &selection.witness_source,
            super::product_receipt::ProductSourceVerification::Retained,
        )?;
        verify_witness_projection(selection, &witness)?;
    }
    Ok(())
}

fn product_consumer_source(
    resolution: &ResolutionOutput,
    subject: &SubjectResolutionAuthority,
) -> anyhow::Result<ResolvedProductConsumerSource> {
    if !matches!(
        resolution.effective_trust_class,
        ryeos_engine::resolution::TrustClass::TrustedProject
            | ryeos_engine::resolution::TrustClass::TrustedBundle
    ) {
        bail!("product consumption requires an admitted signed consumer");
    }
    // Selection testimony names the owner of the declaration-bearing
    // executable, not the project context used to resolve its relationship
    // Configs. A bundled verifier may require a pinned project for those
    // Configs while retaining its bundle provenance in every selected product
    // record. The binding authority separately carries that pinned context.
    if matches!(
        declaring_authority(resolution)?,
        ryeos_engine::external_content::DeclaringAuthority::Bundle(_)
    ) {
        let publisher_fingerprint = resolution
            .root
            .signer_fingerprint
            .clone()
            .context("bundle product consumer has no verified publisher fingerprint")?;
        return Ok(ResolvedProductConsumerSource::InstalledBundle {
            consumer_ref: resolution.root.resolved_ref.clone(),
            publisher_fingerprint,
        });
    }
    // The existing owner supplies exact source closure and source provenance.
    // Strip its effective digest: source testimony cannot embed the D1 which
    // this selection itself will compute.
    use ryeos_state::objects::ExternalContentConsumerAuthority;
    let mut before_realization = resolution.clone();
    before_realization
        .composed
        .derived
        .remove(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY);
    Ok(
        match crate::external_content_admission::consumer_authority(&before_realization, subject)? {
            ExternalContentConsumerAuthority::InstalledBundle {
                consumer_ref,
                publisher_fingerprint,
            } => ResolvedProductConsumerSource::InstalledBundle {
                consumer_ref,
                publisher_fingerprint,
            },
            ExternalContentConsumerAuthority::PinnedProject {
                consumer_ref,
                publisher_fingerprint,
                project_snapshot_hash,
                source_closure,
                ..
            } => ResolvedProductConsumerSource::PinnedProject {
                consumer_ref,
                publisher_fingerprint,
                project_snapshot_hash,
                source_closure,
            },
        },
    )
}

fn verify_witness_projection(
    selection: &ResolvedExternalProductSelection,
    witness: &ryeos_state::external_content::products::publication::VerifiedProductWitness,
) -> anyhow::Result<()> {
    let evidence = &witness.evidence;
    if selection.witness_hash != witness.attestation_hash
        || selection.witness_coordinate != ProductCaptureCoordinate::from_evidence(evidence)?
        || selection.owner_principal != evidence.owner_principal
        || selection.producer != evidence.root_producer
        || selection.manifest_hash != evidence.manifest_hash
        || selection.manifest_kind != evidence.manifest_kind
    {
        bail!("retained product witness contradicts the admitted selection testimony");
    }
    selection
        .relationship
        .validate_compatible_product_evidence(evidence)?;
    Ok(())
}

type ManifestGroups = BTreeMap<(String, String), (Vec<String>, u64)>;

fn manifest_groups(
    witnesses: &BTreeMap<
        String,
        ryeos_state::external_content::products::publication::VerifiedProductWitness,
    >,
    maximum_bytes: u64,
) -> anyhow::Result<ManifestGroups> {
    collect_manifest_groups(
        witnesses.iter().map(|(declaration, witness)| {
            let evidence = &witness.evidence;
            (
                declaration.clone(),
                evidence.manifest_kind.clone(),
                evidence.manifest_hash.clone(),
                evidence.total_bytes,
            )
        }),
        maximum_bytes,
    )
}

fn collect_manifest_groups(
    manifests: impl IntoIterator<Item = (String, String, String, u64)>,
    maximum_bytes: u64,
) -> anyhow::Result<ManifestGroups> {
    let mut groups: ManifestGroups = BTreeMap::new();
    for (declaration_id, kind, hash, total_bytes) in manifests {
        let group = groups
            .entry((kind, hash))
            .or_insert_with(|| (Vec::new(), total_bytes));
        if group.1 != total_bytes {
            bail!("same product manifest has contradictory byte accounting");
        }
        group.0.push(declaration_id);
    }
    let total = groups.values().try_fold(0_u64, |sum, (_, bytes)| {
        sum.checked_add(*bytes)
            .context("product batch byte accounting overflow")
    })?;
    if total > maximum_bytes {
        bail!("product batch exceeds its aggregate import ceiling: {total} > {maximum_bytes}");
    }
    Ok(groups)
}

/// Finish the normal fresh import/bind transaction after exact source selection.
/// The caller keeps its prepared pinned project/source publication alive until
/// this operation returns. This does not accept unverified caller projections.
pub async fn compose_selected_products(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ComposeRetainedProductsRequest,
    resolution: &ResolutionOutput,
    prepared: PreparedProductImports,
) -> anyhow::Result<ComposeRetainedProductsResponse> {
    request.validate()?;
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    if prepared.request_digest
        != lillux::sha256_hex(lillux::canonical_json(&serde_json::to_value(&request)?)?.as_bytes())
    {
        bail!("product composition changed after its exact batch was prepared");
    }
    let subject = request.subject_resolution_authority();
    verify_recovered_selections(
        &state,
        resolution,
        &subject,
        &context.fingerprint,
        &request.selections,
    )?;
    let selected_digest =
        ryeos_engine::external_content::pre_external_realization_consumer_digest(resolution)?;
    let selected = resolved_external_product_selections(resolution)?
        .context("composed product selection disappeared")?;
    if selected != prepared.selections || resolution.root.resolved_ref != request.consumer_ref {
        bail!("product batch lost its exact prepared consumer selection");
    }
    let consumer = crate::external_content_admission::consumer_authority(resolution, &subject)?;
    let mut bindings = Vec::with_capacity(prepared.imports.len());
    for (declaration_ids, imported) in prepared.imports {
        let manifest_kind = imported.manifest_kind.clone();
        let manifest_hash = imported.manifest_hash.clone();
        let total_bytes = imported.total_bytes;
        let binding = super::bind_selected_product_resolution(
            Arc::clone(&state),
            context.clone(),
            resolution,
            &subject,
            imported,
        )
        .await?;
        bindings.push(ProductCompositionBinding {
            declaration_ids,
            manifest_kind,
            manifest_hash,
            total_bytes,
            binding,
        });
    }
    Ok(ComposeRetainedProductsResponse {
        consumer,
        project_context: request.project_context,
        pre_selection_effective_definition_digest: prepared.d0,
        selected_effective_definition_digest: selected_digest,
        selections: request.selections,
        bindings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> ComposeRetainedProductsRequest {
        ComposeRetainedProductsRequest {
            consumer_ref: "config:example/environment".to_owned(),
            project_context: Some(ProductCompositionProjectContext {
                snapshot_hash: "b".repeat(64),
            }),
            selections: vec![ProductSelection {
                declaration_id: "runtime".to_owned(),
                witness_hash: "a".repeat(64),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            }],
            maximum_bytes: 1024,
        }
    }

    #[test]
    fn composition_request_is_exact_and_does_not_carry_derived_authority() {
        let request = request();
        request.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&request.selections).unwrap(),
            json!([{
                "declaration_id": "runtime", "witness_hash": "a".repeat(64),
                "witness_source": {"kind":"local_capture"},
                "qualification_hash": null,
            }])
        );
        for field in [
            "relationship_name",
            "manifest_hash",
            "producer",
            "binding_hash",
            "consumer_project_path",
        ] {
            let mut value = serde_json::to_value(&request).unwrap();
            value[field] = json!("caller assertion");
            assert!(serde_json::from_value::<ComposeRetainedProductsRequest>(value).is_err());
        }
        let mut value = serde_json::to_value(&request).unwrap();
        value.as_object_mut().unwrap().remove("project_context");
        assert!(serde_json::from_value::<ComposeRetainedProductsRequest>(value).is_err());
        let mut value = serde_json::to_value(&request).unwrap();
        value["selections"][0]
            .as_object_mut()
            .unwrap()
            .remove("qualification_hash");
        assert!(serde_json::from_value::<ComposeRetainedProductsRequest>(value).is_err());
    }

    #[test]
    fn composition_refuses_unpinned_or_unsupported_consumer_coordinates() {
        for consumer in [
            "worker:example/consumer@latest",
            "config:example/environment@latest",
            "environment",
        ] {
            let mut request = request();
            request.consumer_ref = consumer.to_owned();
            assert!(request.validate().is_err());
        }
        let mut request = request();
        request.project_context.as_mut().unwrap().snapshot_hash = "B".repeat(64);
        assert!(request.validate().is_err());
        request.project_context.as_mut().unwrap().snapshot_hash = "b".repeat(64);
        request.maximum_bytes = 0;
        assert!(request.validate().is_err());
    }

    #[test]
    fn composition_request_defers_kind_eligibility_to_signed_content_contract() {
        for kind in [
            "tool",
            "graph",
            "config",
            "worker",
            "directive",
            "custom_consumer",
        ] {
            let mut request = request();
            request.consumer_ref = format!("{kind}:example/consumer");
            request.project_context = None;
            request.validate().unwrap();
            assert_eq!(
                request.subject_resolution_authority(),
                SubjectResolutionAuthority::Projectless
            );
        }
    }

    #[test]
    fn relationship_lookup_preserves_admitted_cow_operational_generation_and_root() {
        use ryeos_engine::item_resolution::ResolutionRoots;
        let root = std::path::PathBuf::from("/admitted/exact-source-generation");
        let roots = ResolutionRoots::from_registered(Some(root.clone()), &[]);
        let base = "a".repeat(64);
        for generation in ["b".repeat(64), "c".repeat(64)] {
            let subject = SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash: base.clone(),
                current_operational_generation: generation.clone(),
            };
            let request =
                relationship_resolution_request(&roots, &subject, "config:example/relationships")
                    .unwrap();
            assert_eq!(request.project_root, Some(root.clone()));
            assert_eq!(request.subject_resolution_authority, subject);
            assert_eq!(
                request
                    .subject_resolution_authority
                    .operational_generation(),
                Some(generation.as_str())
            );
            assert_eq!(request.item_ref.to_string(), "config:example/relationships");
            assert_eq!(request.expected_kind.as_deref(), Some("config"));
            let pinned = SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: generation,
            };
            let pinned_request =
                relationship_resolution_request(&roots, &pinned, "config:example/relationships")
                    .unwrap();
            assert_eq!(pinned_request.project_root, request.project_root);
            assert_eq!(pinned_request.subject_resolution_authority, pinned);
        }
    }

    #[test]
    fn relationship_lookup_never_promotes_live_or_loose_roots() {
        use ryeos_engine::item_resolution::ResolutionRoots;
        let roots = ResolutionRoots::from_registered(None, &[]);
        let projectless = relationship_resolution_request(
            &roots,
            &SubjectResolutionAuthority::Projectless,
            "config:example/relationships",
        )
        .unwrap();
        assert_eq!(projectless.project_root, None);
        assert_eq!(
            projectless.subject_resolution_authority,
            SubjectResolutionAuthority::Projectless
        );
        let cow = SubjectResolutionAuthority::CowWorkspace {
            base_snapshot_hash: "a".repeat(64),
            current_operational_generation: "b".repeat(64),
        };
        assert!(
            relationship_resolution_request(&roots, &cow, "config:example/relationships").is_err()
        );
        let loose = ResolutionRoots::from_flat(Some("/loose/project/.ai".into()), vec![]);
        assert!(
            relationship_resolution_request(&loose, &cow, "config:example/relationships").is_err()
        );
        let registered = ResolutionRoots::from_registered(Some("/admitted/source".into()), &[]);
        assert!(
            relationship_resolution_request(
                &registered,
                &SubjectResolutionAuthority::LiveFs,
                "config:example/relationships"
            )
            .is_err()
        );
    }

    #[test]
    fn bundle_product_selection_provenance_remains_bundle_owned_under_pinned_context() {
        use ryeos_engine::contracts::ItemSourceRoot;
        use ryeos_engine::resolution::{
            KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
        };

        let resolution = ResolutionOutput {
            root: ResolvedAncestor {
                requested_id: "qualification/verify".into(),
                resolved_ref: "tool:qualification/verify".into(),
                source_path: "/bundles/standard/.ai/tools/qualification/verify.py".into(),
                source_space: ItemSpace::Bundle,
                source_root: ItemSourceRoot::Bundle {
                    name: "standard".into(),
                },
                trust_class: TrustClass::TrustedBundle,
                signer_fingerprint: Some("a".repeat(64)),
                alias_resolution: None,
                added_by: ResolutionStepName::PipelineInit,
                raw_content: String::new(),
                source_content_digest: "b".repeat(64),
                raw_content_digest: "c".repeat(64),
            },
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: Default::default(),
            effective_trust_class: TrustClass::TrustedBundle,
            composed: KindComposedView::identity(serde_json::json!({
                "executor_id": "@subprocess",
                "config": {"command": "verification"}
            })),
        };
        let source = product_consumer_source(
            &resolution,
            &SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: "d".repeat(64),
            },
        )
        .unwrap();
        assert!(matches!(
            source,
            ResolvedProductConsumerSource::InstalledBundle { .. }
        ));
    }

    #[test]
    fn composition_requires_a_bounded_canonical_complete_selection_request() {
        let mut request = request();
        request.selections.push(request.selections[0].clone());
        assert!(request.validate().is_err());
        request.selections[1].declaration_id = "aaa".to_owned();
        assert!(request.validate().is_err());
        request
            .selections
            .sort_by(|left, right| left.declaration_id.cmp(&right.declaration_id));
        request.validate().unwrap();
        request.selections.clear();
        assert!(request.validate().is_err());
    }

    #[test]
    fn manifest_batch_bound_is_aggregate_but_shared_bytes_are_counted_once() {
        let entry = |id: &str, hash: &str, size: u64| {
            (id.to_owned(), "tree".to_owned(), hash.to_owned(), size)
        };
        let groups = collect_manifest_groups(
            [
                entry("a", "one", 6),
                entry("b", "one", 6),
                entry("c", "two", 4),
            ],
            10,
        )
        .unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[&("tree".to_owned(), "one".to_owned())].0, ["a", "b"]);
        assert!(collect_manifest_groups([entry("a", "one", 6), entry("b", "two", 6)], 10).is_err());
        assert!(collect_manifest_groups([entry("a", "one", 6), entry("b", "one", 7)], 20).is_err());
        assert!(
            collect_manifest_groups(
                [entry("a", "one", u64::MAX), entry("b", "two", 1)],
                u64::MAX
            )
            .is_err()
        );
    }
}
