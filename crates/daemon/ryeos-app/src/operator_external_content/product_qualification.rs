//! Proof construction for independently completed retained-product verifiers.
//!
//! This module does not launch a verifier or grant consumer access. It
//! authenticates an exact current product witness, selects policy only from
//! that witness's signed relationship, projects facts from an already
//! completed admitted verifier, and can publish that immutable testimony.

pub(super) mod runtime_identity;

use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, ItemSourceRoot, ItemSpace, PlanContext, Principal,
    ProjectContext, SubjectResolutionAuthority,
};
use ryeos_engine::external_content::{
    EXTERNAL_REALIZATIONS_DERIVED_KEY, insert_resolved_product_selections,
    resolved_external_product_selections,
};
use ryeos_engine::resolution::{ResolutionOutput, TrustClass};
use ryeos_state::external_content::products::ProductShape;
use ryeos_state::external_content::products::composition::{
    AdmittedProductQualification, ProductSelection, ProductSelectionInput, ProductSelectionInputs,
    ProductSelectionTarget, ResolvedExternalProductSelections,
};
use ryeos_state::external_content::products::publication::{
    ProductCaptureCoordinate, load_product_attestation_value,
};
use ryeos_state::external_content::products::qualification::{
    PRODUCT_QUALIFICATION_EVIDENCE_SCHEMA, ProductQualificationEvidence,
    ProductQualificationPolicySource, ProductQualificationResult, ProductQualificationVerifier,
};
use ryeos_state::external_content::products::qualification_publication::{
    QualificationCoordinate, QualificationWitnessLookup, VerifiedQualificationWitness,
    lookup_qualification_witness_hash_guarded, publish_qualification_witness,
};
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use ryeos_state::objects::{
    Attestation, ExternalContentKind, ExternalContentMode, ExternalContentRealizationSet,
    ThreadSnapshot, ThreadStatus,
};
use serde::{Deserialize, Serialize};

use crate::handler_context::HandlerContext;
use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::state::AppState;

const QUALIFICATION_POLICY_FIELD: &str = "product_qualification_policy";

/// Exact current identity reconstructed without publishing or launching a
/// verifier. Execution mechanics come from its signed schema, not its kind name.
pub(super) struct CurrentBundleVerifierIdentity {
    pub effective_definition_digest: String,
    pub artifact_identity: ryeos_state::objects::AdmittedLaunchArtifactIdentity,
    resolution: ResolutionOutput,
    realizations: ExternalContentRealizationSet,
}

#[derive(Clone, Copy)]
enum CurrentVerifierContent<'a> {
    Root(Option<&'a ResolvedExternalProductSelections>),
    Inherited(&'a ExternalContentRealizationSet),
}

#[derive(Clone, Copy)]
struct CurrentVerifierContext<'a> {
    content: CurrentVerifierContent<'a>,
    logical_project_root: Option<&'a std::path::Path>,
    /// Binding identity belongs to the authority sealed at verifier admission.
    /// A Bundle-owned executable can still have generation-scoped product
    /// bindings when its relationship Configs came from a pinned project.
    binding_subject_authority: Option<&'a ryeos_engine::contracts::SubjectResolutionAuthority>,
}

pub(super) mod execution_evidence;

/// Coordinates only. Policy, claims, verifier identity, subject and result all
/// come from authenticated retained state rather than this request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationRequest {
    pub witness_hash: String,
    pub witness_source: ProductWitnessSource,
    pub relationship_name: String,
    pub verifier_chain_root_id: String,
    pub verifier_thread_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductQualificationResponse {
    pub qualification_hash: String,
    pub coordinate_id: String,
    pub idempotent: bool,
}

impl ProductQualificationRequest {
    fn validate(&self) -> anyhow::Result<()> {
        require_canonical_hash("product witness", &self.witness_hash)?;
        self.witness_source.validate()?;
        require_bounded_name("product relationship", &self.relationship_name)?;
        ryeos_runtime::validate_runtime_thread_id(&self.verifier_chain_root_id)
            .map_err(anyhow::Error::msg)?;
        ryeos_runtime::validate_runtime_thread_id(&self.verifier_thread_id)
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }
}

/// Build validated node testimony without signing or head mutation. The
/// public qualification owner below invokes the same proof before publishing.
pub async fn prove(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductQualificationRequest,
) -> anyhow::Result<ProductQualificationEvidence> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    tokio::task::spawn_blocking(move || prove_blocking(state, context, request))
        .await
        .context("product qualification proof task stopped")?
}

/// Prove and publish one immutable qualification decision. The request cannot
/// state evidence or expiry. This first lane uses a non-expiring attestation;
/// every fresh consumer must still pass the exact current-policy/verifier gate.
pub async fn qualify(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductQualificationRequest,
) -> anyhow::Result<ProductQualificationResponse> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    tokio::task::spawn_blocking(move || qualify_blocking(state, context, request))
        .await
        .context("product qualification publication task stopped")?
}

fn prove_blocking(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductQualificationRequest,
) -> anyhow::Result<ProductQualificationEvidence> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    prove_with_guard(&state, &context, &request, &authority, &guard, limits)
}

fn qualify_blocking(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductQualificationRequest,
) -> anyhow::Result<ProductQualificationResponse> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let evidence = prove_with_guard(&state, &context, &request, &authority, &guard, limits)?;
    let coordinate = QualificationCoordinate::from_evidence(&evidence)?;
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let attestation = evidence.sign_attestation(&signer, lillux::time::iso8601_now(), None)?;
    // Heavy source, capsule, result, realization and manifest checks are done
    // before admitting a writer. The permit covers only immutable publication.
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire product qualification publication permit: {error}")
        })?;
    let published = publish_qualification_witness(
        &authority,
        &coordinate,
        &attestation,
        limits,
        &signer,
        &guard,
    )?;
    Ok(ProductQualificationResponse {
        qualification_hash: published.witness.attestation_hash,
        coordinate_id: published.witness.coordinate_id,
        idempotent: published.reused_existing,
    })
}

fn prove_with_guard(
    state: &AppState,
    context: &HandlerContext,
    request: &ProductQualificationRequest,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
) -> anyhow::Result<ProductQualificationEvidence> {
    authority.ensure_guard(guard)?;
    let product = super::product_receipt::load_product_source(
        state,
        authority,
        guard,
        limits,
        &context.fingerprint,
        &request.witness_hash,
        &request.witness_source,
        super::product_receipt::ProductSourceVerification::Fresh,
    )?;
    let relationship = product
        .evidence
        .relationships
        .select(&request.relationship_name)?
        .clone();
    relationship.validate_product_evidence(&product.evidence)?;
    let policy_ref = relationship
        .qualification
        .policy_ref
        .as_deref()
        .context("selected product relationship has no qualification policy")?;
    let policy_source = resolve_current_bundle_qualification_policy(state, policy_ref)?;
    let root = state
        .state_store
        .get_authoritative_root_thread_snapshot(&request.verifier_chain_root_id)?
        .context("qualification verifier root does not exist")?;
    // Read the snapshot and complete successor presence at one signed head.
    // Deferred accounting may legitimately follow a terminal or continuation.
    let (terminal, has_continuation, _current_chain_head_hash) = state
        .state_store
        .get_authoritative_thread_snapshot_with_continuation_presence(
            &request.verifier_chain_root_id,
            &request.verifier_thread_id,
        )?
        .context("qualification verifier terminal does not exist")?;
    authorize_verifier_terminal(
        &root,
        &terminal,
        has_continuation,
        &request,
        &context.fingerprint,
    )?;
    let capsule_hash = terminal
        .admitted_launch_capsule_hash
        .as_deref()
        .expect("authorized verifier terminal has a capsule");
    let cas = authority.cas_store()?;
    let capsule = ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(
        ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &cas,
            capsule_hash,
            limits.max_object_bytes,
        )?,
    )?;
    let realization = capsule.verify_retained_execution_realization(
        &cas,
        &authority.large_object_store()?,
        authority.trust_store(),
    )?;
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
        &capsule,
    )?;
    let admitted_resolution = sealed.admitted_effective_resolution()?;
    require_reproducible_current_verifier_lane(&admitted_resolution.composed.derived, true)?;
    let (root_selectors, verifier_root_selections) = retained_verifier_root_selections(
        sealed.product_selections(),
        &admitted_resolution,
        &policy_source.policy.subject_declaration_id,
    )?;
    if !root_selectors.is_empty() {
        super::product_composition::verify_recovered_selections_guarded(
            state,
            authority,
            guard,
            limits,
            &admitted_resolution,
            sealed.resolution_subject_authority(),
            &context.fingerprint,
            &root_selectors,
        )?;
    }
    // A direct Tool's executable source is a first-class admitted closure.
    // Validate that retained closure before comparing it with the freshly
    // re-admitted current Bundle definition below; the capsule hash alone is
    // not permission to trust missing or contradictory source objects.
    let _retained_source = crate::source_closure_admission::recover_source_closure(
        state,
        &state.engine,
        &admitted_resolution,
    )?;
    let current_verifier = resolve_current_bundle_verifier_identity_against_admitted(
        state,
        authority,
        guard,
        context,
        &policy_source.policy.verifier_ref,
        &policy_source.policy.verifier_parameters,
        CurrentVerifierContext {
            content: CurrentVerifierContent::Root(verifier_root_selections.as_ref()),
            logical_project_root: ProductQualificationVerifier::project_root_from_closure(
                &capsule.execution_closure,
            )?
            .as_deref(),
            binding_subject_authority: Some(sealed.resolution_subject_authority()),
        },
        Some(&admitted_resolution),
    )?;
    let current_artifact_identity = &current_verifier.artifact_identity;
    let admitted_parameters_digest = sealed.admitted_parameters_digest()?;
    let launch_authority_digest = capsule.launch_authority_digest()?;
    let artifact_identity_digest = capsule.launch_authority().artifact_identity_digest()?;
    let effective_definition_digest = sealed.effective_definition_digest().as_str();
    require_verifier_consistency("terminal.item_ref", terminal.item_ref == sealed.item_ref())?;
    require_verifier_consistency(
        "terminal.project_authority",
        terminal.project_authority == capsule.project_authority,
    )?;
    require_verifier_consistency(
        "policy.verifier_ref",
        sealed.item_ref() == policy_source.policy.verifier_ref,
    )?;
    require_verifier_consistency(
        "current.effective_definition_digest",
        effective_definition_digest == current_verifier.effective_definition_digest,
    )?;
    require_verifier_consistency(
        "policy.admitted_parameters_digest",
        admitted_parameters_digest == policy_source.policy.admitted_parameters_digest()?,
    )?;
    require_verifier_consistency(
        "realization.effective_definition_digest",
        realization.effective_definition_digest == effective_definition_digest,
    )?;
    require_verifier_consistency(
        "realization.launch_authority_digest",
        realization.launch_authority_digest == launch_authority_digest,
    )?;
    require_verifier_consistency(
        "realization.artifact_identity_digest",
        realization.artifact_identity_digest == artifact_identity_digest,
    )?;
    if current_artifact_identity != &capsule.artifact_identity {
        // Labels identify the failed authority without exposing parameters,
        // source text, plan environment, or an entire artifact document.
        let fields = differing_artifact_fields(
            &serde_json::to_value(&capsule.artifact_identity)?,
            &serde_json::to_value(current_artifact_identity)?,
        );
        bail!("qualification verifier consistency failed: current.artifact_identity ({fields})");
    }
    require_verifier_consistency(
        "realization.content_hash",
        realization.content_hash()? == capsule.execution_realization_hash,
    )?;
    let realization_set = capsule
        .external_realization_set()?
        .context("qualification verifier admitted no external-content realizations")?;
    require_exact_pinned_subject(
        &realization_set,
        &policy_source.policy.subject_declaration_id,
        product.evidence.manifest_hash.as_str(),
        product.evidence.declaration.shape,
        product.evidence.entry_count,
        product.evidence.total_bytes,
    )?;

    let (projected_result, execution_proof) = execution_evidence::prove(
        state,
        authority,
        guard,
        limits,
        context,
        &terminal,
        &capsule,
        &admitted_resolution,
        &current_verifier,
        &policy_source.policy.subject_declaration_id,
        product.evidence.manifest_hash.as_str(),
    )?;
    let result = ProductQualificationResult::from_value(&projected_result)?;
    result.validate_claims_for(
        &policy_source.policy,
        &relationship.qualification.required_claims,
    )?;
    let result_digest = result.digest()?;
    let subject_declaration_id = policy_source.policy.subject_declaration_id.clone();
    let product_coordinate = ProductCaptureCoordinate::from_evidence(&product.evidence)?;
    let product_witness_hash = product.attestation_hash;
    let subject_manifest_hash = product.evidence.manifest_hash;
    let evidence = ProductQualificationEvidence {
        schema: PRODUCT_QUALIFICATION_EVIDENCE_SCHEMA.to_owned(),
        product_witness_hash,
        witness_source: request.witness_source.clone(),
        product_coordinate,
        policy_source,
        verifier_root_selections,
        execution_proof,
        verifier: ProductQualificationVerifier {
            chain_root_id: request.verifier_chain_root_id.clone(),
            thread_id: request.verifier_thread_id.clone(),
            admitted_launch_capsule_hash: capsule_hash.to_owned(),
            canonical_ref: sealed.item_ref().to_owned(),
            effective_definition_digest: effective_definition_digest.to_owned(),
            exact_program_hash: capsule.exact_program_hash.clone(),
            admitted_parameters_digest,
            launch_authority_digest,
            execution_realization_hash: capsule.execution_realization_hash.clone(),
            artifact_identity: capsule.artifact_identity.clone(),
            admitted_project_root: ProductQualificationVerifier::project_root_from_closure(
                &capsule.execution_closure,
            )?,
            substrate_identity_hash: realization.substrate_identity_hash,
            subject_declaration_id,
            subject_manifest_hash,
            terminal_snapshot_hash: ryeos_state::objects::thread_snapshot::hash_snapshot(
                &terminal,
            )?,
            result_digest,
        },
        result,
    };
    evidence.validate_current_policy(
        &evidence.policy_source,
        &current_verifier.effective_definition_digest,
        &relationship.qualification.required_claims,
    )?;
    Ok(evidence)
}

fn authorize_verifier_terminal(
    root: &ThreadSnapshot,
    terminal: &ThreadSnapshot,
    has_continuation: bool,
    request: &ProductQualificationRequest,
    operator: &str,
) -> anyhow::Result<()> {
    if root.thread_id != request.verifier_chain_root_id
        || root.chain_root_id != request.verifier_chain_root_id
        || root.requested_by.as_deref() != Some(operator)
        || terminal.chain_root_id != request.verifier_chain_root_id
        || terminal.thread_id != request.verifier_thread_id
        || terminal.requested_by.as_deref() != Some(operator)
    {
        bail!("qualification verifier is not owned at the requested coordinate");
    }
    if terminal.status != ThreadStatus::Completed
        || terminal.error.is_some()
        || terminal.finished_at.is_none()
        || terminal.admitted_launch_capsule_hash.is_none()
        || terminal.result.is_none()
    {
        bail!("qualification verifier has no successful typed terminal authority");
    }
    if has_continuation {
        bail!("qualification verifier terminal has a continuation successor");
    }
    Ok(())
}

fn retained_verifier_root_selections(
    inputs: &[ProductSelectionInput],
    admitted_resolution: &ResolutionOutput,
    subject_declaration_id: &str,
) -> anyhow::Result<(
    Vec<ProductSelection>,
    Option<ResolvedExternalProductSelections>,
)> {
    let retained = resolved_external_product_selections(admitted_resolution)?;
    match_retained_verifier_root_selections(inputs, retained, subject_declaration_id)
}

fn match_retained_verifier_root_selections(
    inputs: &[ProductSelectionInput],
    retained: Option<ResolvedExternalProductSelections>,
    subject_declaration_id: &str,
) -> anyhow::Result<(
    Vec<ProductSelection>,
    Option<ResolvedExternalProductSelections>,
)> {
    ryeos_state::external_content::products::composition::validate_product_selection_inputs(
        &inputs.to_vec(),
    )?;
    let mut selectors = Vec::with_capacity(inputs.len());
    for input in inputs {
        match &input.target {
            ProductSelectionTarget::Root {} => selectors.push(input.selection.clone()),
            ProductSelectionTarget::ContentDependency { .. }
            | ProductSelectionTarget::WorkloadExecution { .. } => bail!(
                "qualification verifier uses a prepared content-dependency product selection, whose current authority is unsupported"
            ),
        }
    }
    match (selectors.is_empty(), retained) {
        (true, None) => Ok((selectors, None)),
        (true, Some(_)) => bail!(
            "qualification verifier retained product selections without sealed root selectors"
        ),
        (false, None) => bail!(
            "qualification verifier sealed root selectors without admitted product selections"
        ),
        (false, Some(retained)) => {
            if retained.len() != selectors.len() {
                bail!("qualification verifier root selection count changed after admission");
            }
            for selector in &selectors {
                let selected = retained
                    .get(&selector.declaration_id)
                    .context("qualification verifier lost a sealed root selection")?;
                if selected.witness_hash != selector.witness_hash
                    || selected
                        .qualification
                        .as_ref()
                        .map(|proof| &proof.attestation_hash)
                        != selector.qualification_hash.as_ref()
                    || (selector.declaration_id == subject_declaration_id
                        && (selector.qualification_hash.is_some()
                            || selected.qualification.is_some()
                            || selected.relationship.qualification.policy_ref.is_some()))
                {
                    bail!(
                        "qualification verifier root subject must be the exact unqualified sealed product selection"
                    );
                }
            }
            Ok((selectors, Some(retained)))
        }
    }
}

fn require_exact_pinned_subject(
    realizations: &ExternalContentRealizationSet,
    declaration_id: &str,
    manifest_hash: &str,
    shape: ProductShape,
    entry_count: usize,
    total_bytes: u64,
) -> anyhow::Result<()> {
    realizations.validate()?;
    let mut matching = realizations
        .iter()
        .filter(|realization| realization.id == declaration_id);
    let subject = matching
        .next()
        .context("qualification verifier did not admit the policy subject declaration")?;
    if matching.next().is_some() {
        bail!("qualification verifier admitted the subject declaration more than once");
    }
    let kind = match shape {
        ProductShape::File => ExternalContentKind::File,
        ProductShape::Tree => ExternalContentKind::Tree,
    };
    if subject.mode != ExternalContentMode::Pinned
        || subject.manifest_hash != manifest_hash
        || subject.kind != kind
        || subject.entry_count != entry_count
        || subject.total_bytes != total_bytes
    {
        bail!("qualification verifier did not admit the exact fixed-pin product subject");
    }
    Ok(())
}

/// Resolve the current signed policy in the deliberately narrow projectless
/// Bundle lane. Project Config policy needs an exact retained context owner and
/// is refused here rather than falling back to live source.
pub(super) fn resolve_current_bundle_qualification_policy(
    state: &AppState,
    policy_ref: &str,
) -> anyhow::Result<ProductQualificationPolicySource> {
    let resolution = resolve_current_trusted_bundle(state, policy_ref, Some("config"))?;
    let policy = ryeos_state::external_content::products::qualification::ProductQualificationPolicy::from_value(
        resolution
            .composed
            .composed
            .get(QUALIFICATION_POLICY_FIELD)
            .context("qualification Config has no product_qualification_policy")?,
    )?;
    let source = ProductQualificationPolicySource {
        canonical_ref: resolution.root.resolved_ref.clone(),
        raw_content_digest: resolution.root.raw_content_digest.clone(),
        effective_definition_digest: resolution
            .effective_definition_digest()?
            .as_str()
            .to_owned(),
        publisher_fingerprint: resolution
            .root
            .signer_fingerprint
            .clone()
            .context("trusted qualification policy has no publisher")?,
        policy,
    };
    source.validate()?;
    Ok(source)
}

/// Re-resolve the exact current verifier from its signed Bundle source.
/// Retained root selections are application-authenticated before this helper;
/// this owner rechecks their common current D0, current binding heads and full
/// manifest closures before deriving D1 and the realized D2 identity.
pub(super) fn resolve_current_bundle_verifier_identity_for_evidence(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    context: &HandlerContext,
    verifier_ref: &str,
    verifier_parameters: &serde_json::Value,
    evidence: &ProductQualificationEvidence,
) -> anyhow::Result<CurrentBundleVerifierIdentity> {
    evidence.validate()?;
    let cas = authority.cas_store()?;
    let capsule = ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(
        ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &cas,
            &evidence.verifier.admitted_launch_capsule_hash,
            limits.max_object_bytes,
        )?,
    )?;
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
        &capsule,
    )?;
    let admitted_resolution = sealed.admitted_effective_resolution()?;
    if sealed.item_ref() != evidence.verifier.canonical_ref
        || sealed.effective_definition_digest().as_str()
            != evidence.verifier.effective_definition_digest
        || sealed.admitted_parameters_digest()? != evidence.verifier.admitted_parameters_digest
        || capsule.launch_authority_digest()? != evidence.verifier.launch_authority_digest
        || capsule.artifact_identity != evidence.verifier.artifact_identity
    {
        bail!("qualification evidence contradicts its admitted verifier capsule");
    }
    let (_, retained_selections) = retained_verifier_root_selections(
        sealed.product_selections(),
        &admitted_resolution,
        &evidence.policy_source.policy.subject_declaration_id,
    )?;
    if retained_selections != evidence.verifier_root_selections {
        bail!("qualification evidence changed its admitted verifier selections");
    }
    resolve_current_bundle_verifier_identity_against_admitted(
        state,
        authority,
        guard,
        context,
        verifier_ref,
        verifier_parameters,
        CurrentVerifierContext {
            content: CurrentVerifierContent::Root(retained_selections.as_ref()),
            logical_project_root: evidence.verifier.admitted_project_root.as_deref(),
            binding_subject_authority: Some(sealed.resolution_subject_authority()),
        },
        Some(&admitted_resolution),
    )
}

fn current_root_selection_inputs(
    selections: Option<&ResolvedExternalProductSelections>,
) -> anyhow::Result<ProductSelectionInputs> {
    let Some(selections) = selections else {
        return Ok(Vec::new());
    };
    let inputs = selections
        .iter()
        .map(|(_, selected)| ProductSelectionInput {
            target: ProductSelectionTarget::Root {},
            selection: ProductSelection {
                declaration_id: selected.declaration_id.clone(),
                witness_hash: selected.witness_hash.clone(),
                witness_source: selected.witness_source.clone(),
                qualification_hash: selected
                    .qualification
                    .as_ref()
                    .map(|proof| proof.attestation_hash.clone()),
            },
        })
        .collect();
    ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(
        inputs,
    )
}

fn current_verifier_binding_subject_authority<'a>(
    admitted: Option<&'a SubjectResolutionAuthority>,
    projectless: &'a SubjectResolutionAuthority,
) -> &'a SubjectResolutionAuthority {
    admitted.unwrap_or(projectless)
}

#[allow(clippy::too_many_arguments)]
fn resolve_current_bundle_verifier_identity_against_admitted(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    context: &HandlerContext,
    verifier_ref: &str,
    verifier_parameters: &serde_json::Value,
    verifier_context: CurrentVerifierContext<'_>,
    admitted_resolution: Option<&ResolutionOutput>,
) -> anyhow::Result<CurrentBundleVerifierIdentity> {
    authority.ensure_guard(guard)?;
    crate::operator_authority::require_admitted_operator(state, context)?;
    state.engine.with_checked_bundle_generation(|_| {
        resolve_current_bundle_verifier_identity_in_generation(
            state,
            authority,
            guard,
            context,
            verifier_ref,
            verifier_parameters,
            verifier_context,
            admitted_resolution,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn resolve_current_bundle_verifier_identity_in_generation(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    context: &HandlerContext,
    verifier_ref: &str,
    verifier_parameters: &serde_json::Value,
    verifier_context: CurrentVerifierContext<'_>,
    admitted_resolution: Option<&ResolutionOutput>,
) -> anyhow::Result<CurrentBundleVerifierIdentity> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(verifier_ref)?;
    if canonical.to_string() != verifier_ref || canonical.suffix.is_some() {
        bail!("qualification verifier must be an exact canonical Bundle ref");
    }
    let kind = canonical.kind.clone();
    let (verifier_root_selections, inherited) = match verifier_context.content {
        CurrentVerifierContent::Root(selections) => (selections, None),
        CurrentVerifierContent::Inherited(realizations) => (None, Some(realizations)),
    };
    let projectless_binding_authority = SubjectResolutionAuthority::Projectless;
    let binding_subject_authority = current_verifier_binding_subject_authority(
        verifier_context.binding_subject_authority,
        &projectless_binding_authority,
    );
    let product_selections = current_root_selection_inputs(verifier_root_selections)?;
    let plan_context = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: context.fingerprint.clone(),
            scopes: context.scopes.clone(),
        }),
        project_context: ProjectContext::None,
        subject_resolution_authority: SubjectResolutionAuthority::Projectless,
        current_site_id: state.threads.site_id().to_owned(),
        // Configured-operator forwarding is not delegated execution. Retain
        // the authenticated source site without manufacturing local origin.
        origin_site_id: context.execution_origin(state.threads.site_id()),
        execution_hints: ExecutionHints::default(),
        scheduled_fire: None,
        // This remains a threadless proof, but plan reconstruction must use
        // execution semantics rather than a validation-only plan variant.
        validate_only: false,
    };
    let project_binding = crate::thread_lifecycle::AdmittedProjectBinding::explicit_projectless(
        &state.engine,
        &plan_context,
    )?;
    // Admit the exact current subject before choosing its execution route.
    // A signed delegated runtime need not declare a root executor chain.
    let preflight = crate::thread_lifecycle::preflight_root_execution(
        crate::thread_lifecycle::ResolveRootExecutionParams {
            engine: &state.engine,
            plan_context,
            project_binding,
            node_history_policy: state.node_history_policy()?,
            item_ref: verifier_ref,
            ref_bindings: std::collections::BTreeMap::new(),
            product_selections,
            launch_mode: "wait",
            parameters: verifier_parameters.clone(),
            usage_subject: None,
            usage_subject_asserted_by: None,
            creates_chain_root: true,
        },
    )
    .context("admit current qualification verifier root")?;
    let admission = &preflight.root_admission;
    let verified = admission.verified_subject();
    let mut resolution = admission.resolution_output().clone();
    require_reproducible_current_verifier_lane(&resolution.composed.derived, false)?;
    if verified.trust_class != ryeos_engine::contracts::TrustClass::Trusted
        || verified.resolved.canonical_ref.to_string() != resolution.root.resolved_ref
        || verified.resolved.content_hash != resolution.root.source_content_digest
        || verified.resolved.raw_content_digest != resolution.root.raw_content_digest
        || verified.resolved.source_space != resolution.root.source_space
        || verified.resolved.source_root != resolution.root.source_root
        || verified.signer.as_ref().map(|signer| signer.0.as_str())
            != resolution.root.signer_fingerprint.as_deref()
    {
        bail!("current qualification verifier is not an exact trusted Bundle source");
    }

    // Re-run the same source-closure owner against current signed source. Live
    // locators, captured content and prepared content dependencies remain
    // unsupported; root product selections are retained typed recipe facts,
    // not a request to reopen their historical Config sources.
    let contract = state
        .engine
        .kinds
        .get(&kind)
        .and_then(|schema| schema.external_content_contract());
    if inherited.is_some() {
        let declarer = ryeos_engine::external_content::declaring_authority(&resolution)?;
        if ryeos_engine::external_content::effective_external_content_declarations(
            &resolution,
            contract,
            declarer,
        )?
        .is_some()
        {
            bail!("qualification probe must inherit its inputs without additional declarations");
        }
    } else if verifier_root_selections.is_none() {
        let declarer = ryeos_engine::external_content::declaring_authority(&resolution)?;
        let declarations = ryeos_engine::external_content::effective_external_content_declarations(
            &resolution,
            contract,
            declarer,
        )?
        .context("qualification verifier declares no fixed-pin subject")?;
        if declarations.is_empty()
            || declarations.iter().any(|declaration| {
                declaration.mode != ryeos_engine::external_content::ExternalContentMode::Pinned
                    || declaration.locator.is_some()
                    || declaration.digest.is_none()
            })
        {
            bail!(
                "qualification verifier current-identity checks support only locator-free fixed pins"
            );
        }
    }
    let roots = state.engine.resolution_roots(None);
    let staged_roots = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(guard, "qualification-verifier-recheck")?;
    let mut publication = Some(ryeos_state::PendingCasPublication::new(
        authority.try_clone()?,
        staged_roots,
    ));
    let source_contract = state
        .engine
        .kinds
        .get(&kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.source_closure.as_ref());
    let source_policy = if source_contract.is_some() {
        let executor_id = resolution
            .composed
            .composed
            .get("executor_id")
            .and_then(serde_json::Value::as_str)
            .context("qualification Tool has no executor chain")?;
        ryeos_engine::launch::plan_builder::resolve_executor_source_policy(
            executor_id,
            &resolution.root.source_path,
            &kind,
            &state.engine.kinds,
            &state.engine.parser_dispatcher,
            &roots,
            &state.engine.trust_store,
            &state.engine.node_trust_store,
            None,
        )?
    } else {
        None
    };
    // The Tool kind's item-namespace source contract is a ceiling, not a
    // requirement that every executor chain declare adjacent source. Mirror
    // ordinary direct finalization: a chain with no signed source policy has
    // no source closure, while a declared policy is captured by this owner.
    // The complete D2 comparison below still refuses a current/admitted
    // disagreement in either direction.
    let captured_source = crate::source_closure_admission::admit_source_closure_in_publication(
        state,
        &state.engine,
        &kind,
        &mut resolution,
        &roots,
        None,
        source_policy.as_ref(),
        &mut publication,
        None,
    )?;
    let preselection_snapshots =
        crate::effective_program_preparation::prepare_preselection_effective_program(
            &state.engine,
            &verified.resolved,
            &mut resolution,
            &roots,
            &state.engine.trust_store,
            None,
        )?;
    if let Some(admitted) = admitted_resolution
        && resolution
            .composed
            .derived
            .get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY)
            != admitted
                .composed
                .derived
                .get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY)
    {
        bail!("qualification verifier current hook plan differs from its admitted capsule");
    }
    let admitted = match verifier_root_selections {
        Some(selections) => {
            let contract = contract
                .context("selected qualification verifier has no external-content contract")?;
            insert_resolved_product_selections(&mut resolution, selections.clone(), contract)?;
            let preview_policy = selected_verifier_preview_policy(contract)?;
            let preview = crate::external_content_admission::preview_portable_content_dependency_with_realizations(
                state,
                &resolution,
                &preview_policy,
                binding_subject_authority,
            )?;
            if !preview.validation.ready_for_admission {
                bail!("selected qualification verifier has no current exact content binding");
            }
            let realized = preview
                .realizations
                .context("selected qualification verifier produced no current realization")?;
            resolution.composed.derived.insert(
                EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
                realized.to_value()?,
            );
            crate::external_content_admission::recover_external_realizations(state, &resolution)?
                .context("selected qualification verifier realization is not retained")?
        }
        None => crate::external_content_admission::admit_external_realizations_in_publication(
            state,
            &state.engine,
            &kind,
            &mut resolution,
            &roots,
            &SubjectResolutionAuthority::Projectless,
            inherited,
            &mut publication,
        )?
        .context("qualification verifier produced no current external realization")?,
    };
    let semantic_projection =
        ryeos_engine::effective_program::take_recovered_effective_program_derived(&mut resolution);
    let validation = state
        .engine
        .effective_validators
        .validate(&kind, &resolution)?;
    let candidate = ryeos_engine::effective_program::relock_recovered_effective_program(
        resolution,
        validation,
        semantic_projection,
    )?;
    let config_roots = state.engine.launch_config_roots(&roots);
    let config_proofs = preselection_snapshots
        .as_ref()
        .map(|snapshots| std::slice::from_ref(&snapshots.dependency_proof))
        .unwrap_or_default();
    let finalization = ryeos_engine::effective_program::prove_finalization_authority(
        &candidate,
        contract,
        config_proofs,
        &config_roots,
        None,
        Some(admitted.finalization_evidence()),
        captured_source
            .as_ref()
            .map(|captured| captured.finalization_evidence()),
    )?;
    let finalized =
        ryeos_engine::effective_program::finalize_effective_program(candidate, finalization)?;
    let execution = state
        .engine
        .kinds
        .get(&kind)
        .and_then(|schema| schema.execution())
        .context("qualification verifier has no signed execution contract")?;
    let artifact_identity = match (&execution.terminator, &execution.delegate) {
        (Some(ryeos_engine::kind_registry::TerminatorDecl::Subprocess { .. }), None) => {
            let resolved_request = admission.execution_request(
                crate::thread_lifecycle::RootExecutionRoute::RootExecutorChain,
                "wait".to_owned(),
                verifier_parameters.clone(),
            )?;
            runtime_identity::reconstruct_current_direct_artifact_identity(
                state,
                authority,
                guard,
                context,
                &resolved_request,
                &finalized,
                verifier_context.logical_project_root,
            )?
        }
        (None, Some(delegation)) => {
            use crate::thread_lifecycle::managed_runtime_identity as managed;
            if verifier_context.logical_project_root.is_some() {
                bail!("managed verifier cannot assert a direct-plan logical root");
            }
            let ryeos_engine::kind_registry::DelegationVia::RuntimeRegistry { serves_kind } =
                &delegation.via;
            // Current managed launch binds the selected runtime to the actual
            // admitted root kind. Do not certify a delegation override that
            // that launch owner would reject.
            if serves_kind.as_deref().is_some_and(|served| served != kind) {
                bail!(
                    "qualification delegation override is not supported by the managed launch owner"
                );
            }
            let selection =
                managed::resolve_current_managed_runtime_selection(&state.engine, None, &kind)?;
            let executor =
                managed::resolve_current_managed_executor_identity(&state.engine, &selection)?;
            managed::managed_runtime_artifact_identity(&selection, &executor)?
        }
        _ => bail!("qualification verifier execution route has no reproducible launch artifact"),
    };
    let realizations = ExternalContentRealizationSet::from_value(
        finalized
            .resolution()
            .composed
            .derived
            .get(EXTERNAL_REALIZATIONS_DERIVED_KEY)
            .context("current verifier lost its admitted inputs")?,
    )?;
    let effective_definition_digest = finalized.effective_definition_digest().as_str().to_owned();
    // Keep the temporary publication and proofs live through finalization and
    // direct artifact reconstruction. Its
    // staged roots are never promoted; the durable binding/product owners
    // remain the authority for the retained bytes.
    drop(admitted);
    drop(captured_source);
    drop(preselection_snapshots);
    authority.ensure_guard(guard)?;
    Ok(CurrentBundleVerifierIdentity {
        effective_definition_digest,
        artifact_identity,
        resolution: finalized.resolution().clone(),
        realizations,
    })
}

fn require_reproducible_current_verifier_lane(
    derived: &std::collections::HashMap<String, serde_json::Value>,
    allow_product_selections: bool,
) -> anyhow::Result<()> {
    if let Some(value) = derived.get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY) {
        let hooks = ryeos_engine::hooks::EffectiveHookPlan::from_value(value)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        if hooks
            .sources
            .iter()
            .any(|source| source.source_space == ItemSpace::Project)
        {
            bail!(
                "qualification verifier uses Project hook authority, whose retained current context is unsupported"
            );
        }
    }
    if derived.contains_key(
        ryeos_engine::content_dependencies::EFFECTIVE_CONTENT_DEPENDENCIES_DERIVED_KEY,
    ) {
        bail!(
            "qualification verifier uses prepared content dependencies, whose current authority is unsupported"
        );
    }
    if !allow_product_selections
        && derived
            .contains_key(ryeos_engine::external_content::EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY)
    {
        bail!(
            "qualification verifier uses prepared product selections, whose current authority is unsupported"
        );
    }
    Ok(())
}

fn selected_verifier_preview_policy(
    contract: &ryeos_engine::kind_registry::KindExternalContentDecl,
) -> anyhow::Result<ryeos_engine::runtime_registry::LaunchContentExternalPolicy> {
    let max_declarations = u16::try_from(contract.max_declarations)
        .context("qualification verifier declaration ceiling exceeds the launch wire")?;
    Ok(
        ryeos_engine::runtime_registry::LaunchContentExternalPolicy {
            allowed_mount_roots: contract.allowed_mount_roots.clone(),
            max_declarations,
            large_content_max_total_bytes: contract.large_content.as_ref().map(|grant| {
                grant
                    .max_total_bytes
                    .unwrap_or(ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES)
            }),
        },
    )
}

fn resolve_current_trusted_bundle(
    state: &AppState,
    item_ref: &str,
    required_kind: Option<&str>,
) -> anyhow::Result<ResolutionOutput> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)?;
    if canonical.to_string() != item_ref
        || canonical.suffix.is_some()
        || required_kind.is_some_and(|kind| canonical.kind != kind)
    {
        bail!("qualification source must be an exact canonical ref of the required kind");
    }
    let expected_kind = canonical.kind.clone();
    let resolution =
        state
            .engine
            .effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
                item_ref: canonical,
                expected_kind: Some(expected_kind),
                project_root: None,
                subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            })?;
    if resolution.root.resolved_ref != item_ref
        || resolution.effective_trust_class != TrustClass::TrustedBundle
        || resolution.root.source_space != ItemSpace::Bundle
        || !matches!(resolution.root.source_root, ItemSourceRoot::Bundle { .. })
        || resolution.root.signer_fingerprint.is_none()
    {
        bail!("qualification source must resolve from a current trusted Bundle");
    }
    Ok(resolution)
}

/// Fresh consumption requires the exact current published qualification and
/// applies expiry. Current-policy eligibility remains a separate caller check.
pub(super) fn load_current_qualification(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    owner_principal: &str,
    qualification_hash: &str,
) -> anyhow::Result<VerifiedQualificationWitness> {
    require_canonical_hash("qualification witness", qualification_hash)?;
    let value = load_product_attestation_value(authority, qualification_hash, limits, guard)?
        .context("qualification witness is absent")?;
    let attestation = Attestation::from_value(&value)?;
    let evidence = ProductQualificationEvidence::verify_attestation_for_owner(
        &attestation,
        state.identity.verifying_key(),
        owner_principal,
    )?;
    let coordinate = QualificationCoordinate::from_evidence(&evidence)?;
    let QualificationWitnessLookup::Found(witness) = lookup_qualification_witness_hash_guarded(
        authority,
        &coordinate,
        qualification_hash,
        state.identity.verifying_key(),
        limits,
        guard,
    )?
    else {
        bail!("qualification witness is not currently published");
    };
    if witness
        .attestation
        .is_expired_at(&lillux::time::iso8601_now())?
    {
        bail!("qualification witness is expired");
    }
    Ok(witness)
}

/// Recovery authenticates the exact retained proof under its caller's CAS
/// guard. It deliberately does not reapply current-head, current-policy, or
/// wall-clock eligibility.
pub(super) fn verify_retained_qualification_guarded(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    proof: &AdmittedProductQualification,
    owner_principal: &str,
) -> anyhow::Result<()> {
    require_canonical_hash(
        "retained qualification attestation",
        &proof.attestation_hash,
    )?;
    proof.evidence.validate()?;
    authority.ensure_guard(guard)?;
    let value = load_product_attestation_value(authority, &proof.attestation_hash, limits, guard)?
        .context("retained qualification attestation is absent")?;
    let attestation = Attestation::from_value(&value)?;
    let evidence = ProductQualificationEvidence::verify_attestation_for_owner(
        &attestation,
        state.identity.verifying_key(),
        owner_principal,
    )?;
    if evidence != proof.evidence {
        bail!("retained qualification attestation contradicts its sealed evidence");
    }
    ryeos_state::external_content::products::qualification_publication::verify_retained_verifier_realization(
        authority,
        &evidence,
        limits,
        guard,
    )?;
    Ok(())
}

fn require_verifier_consistency(field: &'static str, matches: bool) -> anyhow::Result<()> {
    if !matches {
        bail!("qualification verifier consistency failed: {field}");
    }
    Ok(())
}

fn differing_artifact_fields(admitted: &serde_json::Value, current: &serde_json::Value) -> String {
    let (Some(admitted), Some(current)) = (admitted.as_object(), current.as_object()) else {
        return "shape".to_owned();
    };
    admitted
        .keys()
        .chain(current.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|field| admitted.get(*field) != current.get(*field))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn require_canonical_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(value) || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        bail!("{label} is not a canonical digest");
    }
    Ok(())
}

fn require_bounded_name(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        bail!("{label} is not an exact bounded name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn verifier_consistency_diagnostics_refuse_without_disclosing_values() {
        super::require_verifier_consistency("policy.admitted_parameters_digest", true).unwrap();
        let error = super::require_verifier_consistency("policy.admitted_parameters_digest", false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "qualification verifier consistency failed: policy.admitted_parameters_digest"
        );
        let first = serde_json::json!({"execution_plan_hash":"old", "runtime_identity":{"private":"first"}});
        let second = serde_json::json!({"execution_plan_hash":"new", "runtime_identity":{"private":"second"}});
        assert_eq!(
            super::differing_artifact_fields(&first, &second),
            "execution_plan_hash, runtime_identity"
        );
        assert!(super::differing_artifact_fields(&first, &first).is_empty());
    }
    use super::*;
    use std::collections::BTreeMap;

    use ryeos_state::external_content::products::composition::{
        ProductRelationship, ProductRelationshipConsumer, ProductRelationshipProducer,
        ProductRelationshipQualification, ProductRelationshipRequiredProduct,
        RESOLVED_EXTERNAL_PRODUCT_SELECTION_SCHEMA, ResolvedExternalProductSelection,
        ResolvedProductConsumerSource, ResolvedProductDeclaration,
    };
    use ryeos_state::external_content::products::{
        ProductBounds, ProductProducerAdmission, ProductStorage,
    };
    use ryeos_state::objects::canonical_value_digest;
    use ryeos_state::objects::thread_snapshot::ThreadSnapshotBuilder;
    use ryeos_state::objects::{ExternalContentMountRoot, ExternalContentRealization};

    fn request() -> ProductQualificationRequest {
        ProductQualificationRequest {
            witness_hash: "a".repeat(64),
            witness_source: ProductWitnessSource::LocalCapture {},
            relationship_name: "linux_runtime".to_owned(),
            verifier_chain_root_id: "T-verifier".to_owned(),
            verifier_thread_id: "T-verifier".to_owned(),
        }
    }

    fn realizations(
        mode: ExternalContentMode,
        manifest_hash: String,
    ) -> ExternalContentRealizationSet {
        ExternalContentRealizationSet::new(vec![ExternalContentRealization {
            id: "subject".to_owned(),
            kind: ExternalContentKind::Tree,
            mode,
            manifest_hash,
            entry_count: 3,
            total_bytes: 7,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "qualification/subject".to_owned(),
        }])
        .unwrap()
    }

    fn verifier_terminal_fixture() -> (
        ProductQualificationRequest,
        ThreadSnapshot,
        ThreadSnapshot,
        String,
    ) {
        let operator = format!("fp:{}", "9".repeat(64));
        let request = ProductQualificationRequest {
            verifier_chain_root_id: "T-verifier-root".to_owned(),
            verifier_thread_id: "T-verifier-terminal".to_owned(),
            ..request()
        };
        let root = ThreadSnapshotBuilder::new(
            request.verifier_chain_root_id.clone(),
            request.verifier_chain_root_id.clone(),
            "tool",
            "tool:test/verifier",
            "native:test-runtime",
        )
        .requested_by(Some(operator.clone()))
        .build();
        let terminal = ThreadSnapshotBuilder::new(
            request.verifier_thread_id.clone(),
            request.verifier_chain_root_id.clone(),
            "tool",
            "tool:test/verifier",
            "native:test-runtime",
        )
        .status(ThreadStatus::Completed)
        .requested_by(Some(operator.clone()))
        .admitted_launch_capsule_hash("a".repeat(64))
        .started_at(Some("2026-09-08T00:00:00Z".to_owned()))
        .finished_at(Some("2026-09-08T00:00:01Z".to_owned()))
        .result(Some(serde_json::json!({"qualified": true})))
        .build();
        (request, root, terminal, operator)
    }

    fn root_selection() -> ResolvedExternalProductSelection {
        let parameters = serde_json::json!({});
        let producer = ProductRelationshipProducer {
            canonical_ref: "graph:test/build".to_owned(),
            recipe_binding: "product_recipe".to_owned(),
            product_name: "runtime".to_owned(),
            parameters: parameters.clone(),
        };
        let relationship = ProductRelationship {
            name: "runtime_to_verifier".to_owned(),
            producer: producer.clone(),
            consumer: ProductRelationshipConsumer {
                canonical_ref: "tool:test/verifier".to_owned(),
                declaration_id: "subject".to_owned(),
            },
            required_product: ProductRelationshipRequiredProduct {
                shape: ProductShape::Tree,
                storage: ProductStorage::Content,
                bounds: ProductBounds {
                    maximum_entries: 8,
                    maximum_depth: 4,
                    maximum_file_bytes: 1024,
                    maximum_total_bytes: 4096,
                },
            },
            qualification: ProductRelationshipQualification {
                policy_ref: None,
                required_claims: Vec::new(),
            },
        };
        ResolvedExternalProductSelection {
            schema: RESOLVED_EXTERNAL_PRODUCT_SELECTION_SCHEMA.to_owned(),
            declaration_id: "subject".to_owned(),
            relationship_name: relationship.name.clone(),
            relationship_ref: "config:test/recipe".to_owned(),
            relationship_raw_content_digest: "1".repeat(64),
            relationship,
            witness_hash: "2".repeat(64),
            witness_source: ProductWitnessSource::LocalCapture {},
            witness_coordinate: ProductCaptureCoordinate {
                owner_principal: format!("fp:{}", "3".repeat(64)),
                chain_root_id: "T-producer-root".to_owned(),
                thread_id: "T-producer-terminal".to_owned(),
                recipe_binding: producer.recipe_binding.clone(),
                product_name: producer.product_name.clone(),
            },
            qualification: None,
            producer: ProductProducerAdmission {
                canonical_ref: producer.canonical_ref,
                effective_definition_digest: "4".repeat(64),
                exact_program_hash: "5".repeat(64),
                producer_project_snapshot_hash: "6".repeat(64),
                launch_authority_digest: "7".repeat(64),
                admitted_parameters_digest: canonical_value_digest(&parameters).unwrap(),
            },
            owner_principal: format!("fp:{}", "3".repeat(64)),
            consumer_source: ResolvedProductConsumerSource::InstalledBundle {
                consumer_ref: "tool:test/verifier".to_owned(),
                publisher_fingerprint: "8".repeat(64),
            },
            pre_selection_effective_definition_digest: "9".repeat(64),
            manifest_hash: "a".repeat(64),
            manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            declaration: ResolvedProductDeclaration {
                id: "subject".to_owned(),
                kind: ExternalContentKind::Tree,
                manifest_hash: "a".repeat(64),
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: "qualification/subject".to_owned(),
            },
        }
    }

    #[test]
    fn qualification_request_is_closed_and_coordinate_only() {
        request().validate().unwrap();
        let value = serde_json::to_value(request()).unwrap();
        serde_json::from_value::<ProductQualificationRequest>(value.clone()).unwrap();
        let mut missing_source = value.clone();
        missing_source
            .as_object_mut()
            .unwrap()
            .remove("witness_source");
        assert!(serde_json::from_value::<ProductQualificationRequest>(missing_source).is_err());
        let mut restated = value;
        restated["claims"] = serde_json::json!(["compatible"]);
        assert!(serde_json::from_value::<ProductQualificationRequest>(restated).is_err());

        let mut invalid = request();
        invalid.witness_hash = "A".repeat(64);
        assert!(invalid.validate().is_err());
        let mut invalid = request();
        invalid.relationship_name.clear();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn qualification_command_requires_explicit_product_source() {
        let command: ryeos_runtime::command::CommandDef =
            serde_yaml::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../bundles/core/.ai/node/commands/external-content-qualify-product.yaml"
            )))
            .unwrap();
        let service: serde_json::Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/core/.ai/services/external-content/qualify-product.yaml"
        )))
        .unwrap();
        let contract =
            ryeos_runtime::command::InvocationInputContract::from_lightweight_schema_value(
                &service["schema"],
            )
            .unwrap()
            .unwrap();
        let expected = request();
        let bound = ryeos_runtime::arg_binder::bind_argv_with_command_and_contract(
            &[
                expected.witness_hash.clone(),
                r#"{"kind":"local_capture"}"#.to_owned(),
                expected.relationship_name.clone(),
                expected.verifier_chain_root_id.clone(),
                expected.verifier_thread_id.clone(),
            ],
            Some(&command),
            Some(&contract),
        )
        .unwrap();
        let decoded = serde_json::from_value::<ProductQualificationRequest>(bound).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn verifier_subject_must_be_the_exact_fixed_pin() {
        let manifest = "b".repeat(64);
        require_exact_pinned_subject(
            &realizations(ExternalContentMode::Pinned, manifest.clone()),
            "subject",
            &manifest,
            ProductShape::Tree,
            3,
            7,
        )
        .unwrap();

        for (set, id, hash, shape, entries, bytes) in [
            (
                realizations(ExternalContentMode::Captured, manifest.clone()),
                "subject",
                manifest.clone(),
                ProductShape::Tree,
                3,
                7,
            ),
            (
                realizations(ExternalContentMode::Pinned, manifest.clone()),
                "different",
                manifest.clone(),
                ProductShape::Tree,
                3,
                7,
            ),
            (
                realizations(ExternalContentMode::Pinned, manifest.clone()),
                "subject",
                "c".repeat(64),
                ProductShape::Tree,
                3,
                7,
            ),
            (
                realizations(ExternalContentMode::Pinned, manifest.clone()),
                "subject",
                manifest.clone(),
                ProductShape::File,
                3,
                7,
            ),
            (
                realizations(ExternalContentMode::Pinned, manifest.clone()),
                "subject",
                manifest.clone(),
                ProductShape::Tree,
                4,
                7,
            ),
            (
                realizations(ExternalContentMode::Pinned, manifest.clone()),
                "subject",
                manifest,
                ProductShape::Tree,
                3,
                8,
            ),
        ] {
            assert!(require_exact_pinned_subject(&set, id, &hash, shape, entries, bytes).is_err());
        }
    }

    #[test]
    fn verifier_terminal_refuses_an_authoritatively_observed_successor() {
        let (request, root, terminal, operator) = verifier_terminal_fixture();
        authorize_verifier_terminal(&root, &terminal, false, &request, &operator).unwrap();
        assert!(authorize_verifier_terminal(&root, &terminal, true, &request, &operator).is_err());
    }

    #[test]
    fn verifier_terminal_requires_exact_owner_coordinate_and_success() {
        let (request, root, terminal, operator) = verifier_terminal_fixture();
        authorize_verifier_terminal(&root, &terminal, false, &request, &operator).unwrap();

        let mut changed = root.clone();
        changed.thread_id = "T-other-root".to_owned();
        assert!(
            authorize_verifier_terminal(&changed, &terminal, false, &request, &operator,).is_err()
        );
        changed = root.clone();
        changed.chain_root_id = "T-other-root".to_owned();
        assert!(
            authorize_verifier_terminal(&changed, &terminal, false, &request, &operator,).is_err()
        );
        changed = root.clone();
        changed.requested_by = Some(format!("fp:{}", "8".repeat(64)));
        assert!(
            authorize_verifier_terminal(&changed, &terminal, false, &request, &operator,).is_err()
        );

        for mutate in [
            (|snapshot: &mut ThreadSnapshot| snapshot.chain_root_id = "T-other-root".to_owned())
                as fn(&mut ThreadSnapshot),
            |snapshot| snapshot.thread_id = "T-other-terminal".to_owned(),
            |snapshot| snapshot.requested_by = Some(format!("fp:{}", "8".repeat(64))),
            |snapshot| snapshot.status = ThreadStatus::Running,
            |snapshot| snapshot.error = Some(serde_json::json!({"error": "failed"})),
            |snapshot| snapshot.finished_at = None,
            |snapshot| snapshot.admitted_launch_capsule_hash = None,
            |snapshot| snapshot.result = None,
        ] {
            let mut changed = terminal.clone();
            mutate(&mut changed);
            assert!(
                authorize_verifier_terminal(&root, &changed, false, &request, &operator,).is_err()
            );
        }
    }

    #[test]
    fn terminal_result_is_the_typed_inner_payload() {
        let manifest = "d".repeat(64);
        let value = serde_json::json!({
            "schema": ryeos_state::external_content::products::qualification::PRODUCT_QUALIFICATION_RESULT_SCHEMA,
            "subject_manifest_hash": manifest,
            "claims": ["compatible"],
            "probe_evidence": {"format": "elf"},
        });
        ProductQualificationResult::from_value(&value).unwrap();
        assert!(
            ProductQualificationResult::from_value(&serde_json::json!({
                "status": "completed",
                "result": value,
            }))
            .is_err()
        );
    }

    #[test]
    fn current_verifier_lane_refuses_unreproduced_launch_projections() {
        let mut derived = std::collections::HashMap::new();
        derived.insert("authored_extension".to_owned(), serde_json::json!({"v": 1}));
        derived.insert(
            ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
            serde_json::json!([]),
        );
        require_reproducible_current_verifier_lane(&derived, false).unwrap();

        let mut source_closed = derived.clone();
        source_closed.insert(
            ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
            serde_json::json!({"binding_hash": "retained"}),
        );
        require_reproducible_current_verifier_lane(&source_closed, false).unwrap();

        let mut unsupported = derived.clone();
        unsupported.insert(
            ryeos_engine::content_dependencies::EFFECTIVE_CONTENT_DEPENDENCIES_DERIVED_KEY
                .to_owned(),
            serde_json::json!({}),
        );
        assert!(require_reproducible_current_verifier_lane(&unsupported, true).is_err());

        let mut effect_validated = derived.clone();
        effect_validated.insert(
            ryeos_effect_contract::EFFECT_AUTHORIZATIONS_DERIVED_KEY.to_owned(),
            serde_json::json!([]),
        );
        require_reproducible_current_verifier_lane(&effect_validated, true).unwrap();

        let mut selected = derived;
        selected.insert(
            ryeos_engine::external_content::EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY.to_owned(),
            serde_json::json!({}),
        );
        assert!(require_reproducible_current_verifier_lane(&selected, false).is_err());
        require_reproducible_current_verifier_lane(&selected, true).unwrap();

        let hook_plan = |space: &str, trust: &str| {
            serde_json::json!({
                "schema": ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_SCHEMA,
                "owner_kind": "graph",
                "event_contracts": {
                    "completed": {
                        "context_contract": {
                            "schema": ryeos_engine::hooks::HOOK_CONTEXT_SCHEMA,
                            "allowed_roots": ["event"]
                        },
                        "allowed_results": ["observation"]
                    }
                },
                "authored": {"hooks": [], "dispatch_caps": []},
                "builtin": {"hooks": [], "dispatch_caps": []},
                "infrastructure": {"hooks": [], "dispatch_caps": []},
                "context": {"hooks": [], "dispatch_caps": []},
                "operator": {"hooks": [], "dispatch_caps": []},
                "project": {"hooks": [], "dispatch_caps": []},
                "sources": [{
                    "layer": if space == "project" { "project" } else { "builtin" },
                    "canonical_ref": "config:test/hooks",
                    "source_space": space,
                    "trust_class": trust,
                    "signer_fingerprint": "a".repeat(64),
                    "source_raw_content_digest": "b".repeat(64)
                }]
            })
        };
        let mut bundle_hooks = source_closed.clone();
        bundle_hooks.insert(
            ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY.to_owned(),
            hook_plan("bundle", "trusted_bundle"),
        );
        require_reproducible_current_verifier_lane(&bundle_hooks, false).unwrap();
        bundle_hooks.insert(
            ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY.to_owned(),
            hook_plan("project", "trusted_project"),
        );
        assert!(require_reproducible_current_verifier_lane(&bundle_hooks, false).is_err());
    }

    #[test]
    fn selected_verifier_preview_preserves_the_signed_kind_ceiling() {
        let contract = ryeos_engine::kind_registry::KindExternalContentDecl {
            realization_derived: EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
            allowed_roots: vec!["project".to_owned()],
            allowed_mount_roots: vec![ExternalContentMountRoot::ExecutionRuntime],
            max_declarations: 7,
            large_content: Some(ryeos_engine::kind_registry::KindLargeContentGrant {
                max_total_bytes: Some(4096),
            }),
        };
        let policy = selected_verifier_preview_policy(&contract).unwrap();
        assert_eq!(policy.max_declarations, 7);
        assert_eq!(
            policy.allowed_mount_roots,
            vec![ExternalContentMountRoot::ExecutionRuntime]
        );
        assert_eq!(policy.large_content_max_total_bytes, Some(4096));

        let mut unrepresentable = contract;
        unrepresentable.max_declarations = usize::from(u16::MAX) + 1;
        assert!(selected_verifier_preview_policy(&unrepresentable).is_err());
    }

    #[test]
    fn current_selected_verifier_preserves_its_admitted_binding_authority() {
        let projectless = SubjectResolutionAuthority::Projectless;
        let pinned = SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: "a".repeat(64),
        };
        assert_eq!(
            current_verifier_binding_subject_authority(Some(&pinned), &projectless),
            &pinned
        );
        assert_eq!(
            current_verifier_binding_subject_authority(None, &projectless),
            &projectless
        );
    }

    #[test]
    fn dynamic_verifier_subject_requires_exact_unqualified_root_selector() {
        assert_eq!(
            match_retained_verifier_root_selections(&[], None, "subject").unwrap(),
            (Vec::new(), None)
        );
        let selected = root_selection();
        selected.validate().unwrap();
        let retained = ResolvedExternalProductSelections::new(BTreeMap::from([(
            selected.declaration_id.clone(),
            selected.clone(),
        )]))
        .unwrap();
        let root = ProductSelectionInput {
            target: ProductSelectionTarget::Root {},
            selection: ProductSelection {
                declaration_id: selected.declaration_id.clone(),
                witness_hash: selected.witness_hash.clone(),
                witness_source: selected.witness_source.clone(),
                qualification_hash: None,
            },
        };
        let subject = "subject";
        let (selectors, proved) = match_retained_verifier_root_selections(
            &[root.clone()],
            Some(retained.clone()),
            subject,
        )
        .unwrap();
        assert_eq!(selectors, vec![root.selection.clone()]);
        assert_eq!(proved, Some(retained.clone()));

        let mut dependency = root.clone();
        dependency.target = ProductSelectionTarget::ContentDependency {
            binding: "verifier_input".to_owned(),
        };
        assert!(match_retained_verifier_root_selections(
            &[dependency],
            Some(retained.clone()),
            subject,
        )
        .is_err());
        assert!(match_retained_verifier_root_selections(&[root.clone()], None, subject).is_err());
        assert!(
            match_retained_verifier_root_selections(&[], Some(retained.clone()), subject).is_err()
        );

        let mut contradicted = root.clone();
        contradicted.selection.witness_hash = "b".repeat(64);
        assert!(
            match_retained_verifier_root_selections(
                &[contradicted],
                Some(retained.clone()),
                subject,
            )
            .is_err()
        );

        let mut qualified = root;
        qualified.selection.qualification_hash = Some("f".repeat(64));
        assert!(
            match_retained_verifier_root_selections(&[qualified], Some(retained), subject).is_err()
        );
    }
}
