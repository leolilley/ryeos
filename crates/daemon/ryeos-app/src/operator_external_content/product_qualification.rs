//! Proof construction for independently completed retained-product verifiers.
//!
//! This module does not launch a verifier or grant consumer access. It
//! authenticates an exact current product witness, selects policy only from
//! that witness's signed relationship, projects facts from an already
//! completed admitted verifier, and can publish that immutable testimony.

#[cfg(test)]
#[path = "product_qualification/consumer_definition_tests.rs"]
mod consumer_definition_tests;
pub mod launch;
pub(super) mod runtime_identity;

use std::collections::BTreeMap;
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
use ryeos_state::external_content::products::producer_recipe::ProducerExecutableSource;
use ryeos_state::external_content::products::publication::{
    ProductCaptureCoordinate, load_product_attestation_value,
};
use ryeos_state::external_content::products::qualification::{
    PRODUCT_QUALIFICATION_EVIDENCE_SCHEMA, ProductProducerRecipeSourceIdentity,
    ProductQualificationBundleDefinitionIdentity, ProductQualificationConsumerContentIdentity,
    ProductQualificationConsumerDefinitionIdentity,
    ProductQualificationConsumerRuntimeMemberIdentity, ProductQualificationEvidence,
    ProductQualificationPolicySource, ProductQualificationResult, ProductQualificationVerifier,
};
use ryeos_state::external_content::products::qualification_publication::{
    QualificationCoordinate, QualificationWitnessLookup, VerifiedQualificationWitness,
    lookup_qualification_witness_hash_guarded, publish_qualification_witness,
};
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use ryeos_state::external_content::runtime_member::exact_runtime_member_hash;
use ryeos_state::objects::{
    Attestation, ExecutableSearchPathEntry, ExternalContentKind, ExternalContentMode,
    ExternalContentRealizationSet, SOURCE_CLOSURE_DERIVED_KEY, SessionProcessEnvironmentValue,
    ThreadSnapshot, ThreadStatus,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::handler_context::HandlerContext;
use crate::node_policy::sections::execution::NodeExecutionAdmissionPolicy;
use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::state::AppState;

const QUALIFICATION_POLICY_FIELD: &str = "product_qualification_policy";
const PRODUCER_RECIPE_FIELD: &str = "product_producer_recipe";

/// Exact current identity reconstructed without publishing or launching a
/// verifier. Execution mechanics come from its signed schema, not its kind name.
pub(super) struct CurrentBundleVerifierIdentity {
    /// Pre-realization identity of the exact signed Bundle verifier admitted
    /// while resolving the current policy/product relationship.
    pub admitted_definition_digest: String,
    pub effective_definition_digest: String,
    pub artifact_identity: ryeos_state::objects::AdmittedLaunchArtifactIdentity,
    pub request_engine: Arc<ryeos_engine::engine::Engine>,
    // The snapshot path and overlay Engine remain valid through downstream
    // evidence projection and participant identity checks.
    _project_context_lease: Option<Box<dyn QualificationProjectContextLease>>,
    resolution: ResolutionOutput,
    realizations: ExternalContentRealizationSet,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeasuredQualificationAuthority {
    pub qualified_product_witness_hash: String,
    pub qualified_product_owner_principal: String,
    pub qualification_signer_public_key: [u8; 32],
    pub qualification_signer_fingerprint: String,
    pub qualification_policy: ProductQualificationPolicySource,
    pub qualification_verifier_effective_definition_digest: String,
    pub qualification_verifier_artifact_identity:
        ryeos_state::objects::AdmittedLaunchArtifactIdentity,
    pub required_qualification_claims: Vec<String>,
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
    /// Exact sealed launch request owning project and engine authority.
    sealed_request: Option<&'a crate::thread_lifecycle::SealedRootExecutionRequest>,
    project_context_resolver: Option<&'a dyn QualificationProjectContextResolver>,
    /// Fresh selected-slot launch has already passed normal dispatch
    /// preflight. Re-resolve current D1/D2 against that exact retained pinned
    /// admission rather than inventing a projectless context.
    pinned_admission: Option<&'a QualificationPinnedAdmissionContext<'a>>,
}

pub struct QualificationPinnedAdmissionContext<'a> {
    pub plan_context: &'a ryeos_engine::contracts::PlanContext,
    pub request_engine: &'a Arc<ryeos_engine::engine::Engine>,
    pub project_binding: &'a crate::thread_lifecycle::AdmittedProjectBinding,
    pub project_root: &'a Path,
    pub project_authority: &'a ryeos_state::objects::ExecutionProjectAuthority,
}

/// Finalize the verifier identities only after normal root dispatch preflight
/// has admitted the daemon-derived selected Root product under an exact pinned
/// read-only snapshot. D1 comes from root admission; D2 is rebuilt through the
/// same current-verifier finalization path used by qualification proof.
#[allow(clippy::too_many_arguments)]
pub fn finalize_pinned_qualification_launch(
    state: &AppState,
    context: &HandlerContext,
    prepared: &mut launch::PreparedProductQualificationLaunch,
    admission: &crate::thread_lifecycle::RootExecutionAdmission,
    request_engine: &Arc<ryeos_engine::engine::Engine>,
    plan_context: &PlanContext,
    project_binding: &crate::thread_lifecycle::AdmittedProjectBinding,
    project_root: &Path,
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> anyhow::Result<()> {
    use ryeos_state::objects::{ExecutionProjectAuthority, PinnedProjectRealization};

    if !Arc::ptr_eq(admission.request_engine(), request_engine) {
        bail!("qualification finalization Engine differs from the admitted request Engine");
    }
    let snapshot_hash = prepared
        .pinned_snapshot_hash
        .as_deref()
        .context("pinned qualification launch has no selected snapshot")?;
    let ExecutionProjectAuthority::PinnedGeneration {
        snapshot_hash: authority_hash,
        realization: PinnedProjectRealization::ReadOnly,
        workspace_outputs: None,
        ..
    } = project_authority
    else {
        bail!("qualification verifier launch requires read-only pinned authority");
    };
    let admitted_plan = admission.plan_context();
    if authority_hash != snapshot_hash
        || !matches!(
            &plan_context.project_context,
            ProjectContext::SnapshotHash { hash } if hash == snapshot_hash
        )
        || plan_context.subject_resolution_authority
            != (SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: snapshot_hash.to_owned(),
            })
        || admission.project_authority() != project_authority
        || admitted_plan.requested_by != plan_context.requested_by
        || admitted_plan.project_context != plan_context.project_context
        || admitted_plan.subject_resolution_authority != plan_context.subject_resolution_authority
        || admitted_plan.current_site_id != plan_context.current_site_id
        || admitted_plan.origin_site_id != plan_context.origin_site_id
        || admitted_plan.execution_hints != plan_context.execution_hints
        || admitted_plan.scheduled_fire != plan_context.scheduled_fire
        || admitted_plan.validate_only != plan_context.validate_only
        || admission.product_selections() != &prepared.product_selections
        || project_binding.exact_authority() != project_authority
        || project_binding.subject_resolution_authority()
            != &plan_context.subject_resolution_authority
    {
        bail!("qualification root admission differs from its exact read-only snapshot request");
    }
    if prepared.product_selections.len() != 1 {
        bail!("pinned qualification requires exactly one daemon-derived Root product selection");
    }
    let selected_input = &prepared.product_selections[0];
    if !matches!(selected_input.target, ProductSelectionTarget::Root {})
        || selected_input.selection.declaration_id
            != prepared.policy_source.policy.subject_declaration_id
        || selected_input.selection.witness_hash != prepared.product_witness_hash
        || selected_input.selection.witness_source != prepared.witness_source
        || selected_input.selection.qualification_hash.is_some()
    {
        bail!("pinned qualification Root selector differs from signed policy subject and witness");
    }

    // RootExecutionAdmission retains caller-independent selectors, while its
    // ResolutionOutput remains the pre-selection D0 composition. Resolve the
    // selector through the ordinary product-composition authority here: that
    // rechecks the signed relationship, exact pinned-project binding, witness,
    // and consumer slot instead of assuming dispatch preflight projected a
    // resolved selection into the root ResolutionOutput.
    let subject_authority = &plan_context.subject_resolution_authority;
    let roots = request_engine.resolution_roots(Some(project_root.to_path_buf()));
    let selectors = prepared
        .product_selections
        .iter()
        .map(|input| match &input.target {
            ProductSelectionTarget::Root {} => Ok(input.selection.clone()),
            _ => bail!("pinned qualification admission contains a non-Root product selector"),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut selected_resolution = admission.resolution_output().clone();
    let retained = crate::operator_external_content::product_composition::
        select_products_with_project_context_resolver(
            state,
            context,
            request_engine,
            &roots,
            subject_authority,
            &mut selected_resolution,
            &selectors,
            None,
        )?;
    if retained.len() != 1 {
        bail!("pinned qualification admission resolved an unexpected Root selection count");
    }
    let selected = retained
        .get(&prepared.policy_source.policy.subject_declaration_id)
        .context("pinned qualification admission lost its signed inner subject slot")?;
    if selected.witness_hash != prepared.product_witness_hash
        || selected.witness_source != prepared.witness_source
        || selected.manifest_hash != prepared.subject_manifest_hash
        || selected.qualification.is_some()
        || selected.relationship.qualification.policy_ref.is_some()
    {
        bail!(
            "pinned qualification resolved subject differs from the authenticated unqualified witness"
        );
    }

    let admitted_definition_digest = admission
        .resolution_output()
        .effective_definition_digest()?
        .as_str()
        .to_owned();
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let current = resolve_current_bundle_verifier_identity_against_admitted(
        state,
        &authority,
        &guard,
        context,
        &prepared.policy_source.policy.verifier_ref,
        &prepared.policy_source.policy.verifier_parameters,
        CurrentVerifierContext {
            content: CurrentVerifierContent::Root(Some(&retained)),
            // Plan identity uses the same stable root as the admitted direct
            // execution closure. `project_root` remains the exact materialized
            // snapshot used for source and binding admission above.
            logical_project_root: Some(Path::new(
                ryeos_state::objects::ADMITTED_DIRECT_PROJECT_ROOT,
            )),
            binding_subject_authority: Some(&plan_context.subject_resolution_authority),
            sealed_request: None,
            project_context_resolver: None,
            pinned_admission: Some(&QualificationPinnedAdmissionContext {
                plan_context,
                request_engine,
                project_binding,
                project_root,
                project_authority,
            }),
        },
        Some(admission.resolution_output()),
    )?;
    if current.admitted_definition_digest != admitted_definition_digest {
        bail!("pinned qualification verifier current D1 differs from root admission");
    }
    prepared.verifier_admitted_definition_digest = Some(admitted_definition_digest);
    prepared.verifier_realized_definition_digest = Some(current.effective_definition_digest);
    authority.ensure_guard(&guard)?;
    Ok(())
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

/// App-owned boundary for rebuilding a current verifier against the exact
/// immutable project generation sealed by its admitted launch capsule.
/// `ryeos-app` deliberately does not depend on the executor crate; the API
/// implements this using executor's existing read-only snapshot resolver.
pub trait QualificationProjectContextResolver: Send + Sync + 'static {
    fn resolve_read_only_snapshot(
        &self,
        snapshot_hash: &str,
        display_path: &Path,
        operation_id: &str,
    ) -> anyhow::Result<Box<dyn QualificationProjectContextLease>>;
}

/// A retained read-only snapshot materialization. The lease must outlive all
/// current resolution and artifact-identity checks that consume its Engine or
/// path; returning only a pathname would discard the cache-generation guard.
pub trait QualificationProjectContextLease {
    fn snapshot_hash(&self) -> &str;
    fn original_path(&self) -> &Path;
    fn effective_path(&self) -> &Path;
    fn request_engine(&self) -> &Arc<ryeos_engine::engine::Engine>;
    fn pinned_materialization(&self) -> &ryeos_state::PinnedProjectMaterialization;
    fn workspace_lifeline(&self) -> &Arc<crate::temp_dir_guard::TempDirGuard>;
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
    prove_with_project_context_resolver(state, context, request, None).await
}

pub async fn prove_with_project_context_resolver(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductQualificationRequest,
    resolver: Option<Arc<dyn QualificationProjectContextResolver>>,
) -> anyhow::Result<ProductQualificationEvidence> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    tokio::task::spawn_blocking(move || prove_blocking(state, context, request, resolver))
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
    qualify_with_project_context_resolver(state, context, request, None).await
}

pub async fn qualify_with_project_context_resolver(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductQualificationRequest,
    resolver: Option<Arc<dyn QualificationProjectContextResolver>>,
) -> anyhow::Result<ProductQualificationResponse> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    tokio::task::spawn_blocking(move || qualify_blocking(state, context, request, resolver))
        .await
        .context("product qualification publication task stopped")?
}

fn prove_blocking(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductQualificationRequest,
    resolver: Option<Arc<dyn QualificationProjectContextResolver>>,
) -> anyhow::Result<ProductQualificationEvidence> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    prove_with_guard(
        &state,
        &context,
        &request,
        &authority,
        &guard,
        limits,
        resolver.as_deref(),
    )
}

fn qualify_blocking(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductQualificationRequest,
    resolver: Option<Arc<dyn QualificationProjectContextResolver>>,
) -> anyhow::Result<ProductQualificationResponse> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let evidence = prove_with_guard(
        &state,
        &context,
        &request,
        &authority,
        &guard,
        limits,
        resolver.as_deref(),
    )?;
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
    project_context_resolver: Option<&dyn QualificationProjectContextResolver>,
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
    if let Some(consumer_context) = &policy_source.policy.consumer_execution_context {
        consumer_context.validate_relationship_consumer(&relationship.consumer)?;
    }
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
    // A completed Tool is not, by itself, a product-qualification verifier.
    // Only the dedicated operator launch may attach this retained purpose;
    // the planning row binds its caller-retained launch coordinate to the
    // exact root that produced the terminal being considered here.
    let purpose = sealed
        .qualification_purpose()
        .context("qualification verifier has no admitted launch purpose")?;
    let (product_witness_hash, witness_source, relationship_name) =
        purpose.subject.captured_product()?;
    let planning = state
        .state_store
        .launch_planning_record_for_owner(&purpose.launch_id, &context.fingerprint)?
        .context("qualification verifier has no owner-bound launch reservation")?;
    if planning.state != "bound"
        || planning.reserved_thread_id != root.thread_id
        || planning.bound_thread_id.as_deref() != Some(root.thread_id.as_str())
        || purpose.owner_fingerprint != context.fingerprint
        || product_witness_hash != product.attestation_hash
        || witness_source != &request.witness_source
        || relationship_name != request.relationship_name
        || purpose.policy_source != policy_source
        || purpose.subject_declaration_id != policy_source.policy.subject_declaration_id
        || purpose.subject_manifest_hash != product.evidence.manifest_hash
        || purpose.required_claims != relationship.qualification.required_claims
    {
        bail!("qualification verifier purpose differs from its accepted product launch");
    }
    if let Some(content) = &purpose.consumer_content {
        require_retained_consumer_content_closure(authority, guard, limits, content)?;
        // The exact historical inputs are retained, but the applied target,
        // environment, protocol and hosted lifecycle are not yet joined.
        bail!("qualification consumer execution context has no applied runtime parity proof");
    }
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
            sealed_request: Some(&sealed),
            project_context_resolver,
            pinned_admission: None,
        },
        Some(&admitted_resolution),
    )?;
    // A direct Tool's executable source is a first-class admitted closure.
    // Validate it against the Engine reconstructed from the verifier's exact
    // sealed project snapshot, not the node's base Engine. The capsule hash
    // alone is not permission to trust missing or contradictory source objects.
    let _retained_source = crate::source_closure_admission::recover_source_closure(
        state,
        current_verifier.request_engine.as_ref(),
        &admitted_resolution,
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
        "launch_purpose.realized_definition_digest",
        purpose.verifier_realized_definition_digest == current_verifier.effective_definition_digest,
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

    let (projected_result, execution_proof, process_settlement) = execution_evidence::prove(
        state,
        authority,
        guard,
        limits,
        context,
        &terminal,
        &capsule,
        &admitted_resolution,
        &current_verifier,
        &purpose.execution_view()?,
        project_context_resolver,
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
        consumer_content: purpose.consumer_content.clone(),
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
            process_settlement_witness_digest: process_settlement
                .as_ref()
                .map(|(digest, _)| digest.clone()),
            process_settlement_authority: process_settlement.map(|(_, authority)| authority),
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

fn require_retained_consumer_content_closure(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    content: &ProductQualificationConsumerContentIdentity,
) -> anyhow::Result<()> {
    authority.ensure_guard(guard)?;
    let roots = std::iter::once(content.worker_source.binding_hash.clone()).chain(
        content
            .worker_literals
            .iter()
            .chain(content.environment_realizations.iter())
            .map(|realized| realized.manifest_hash.clone()),
    );
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &authority.cas_store()?,
        roots,
        limits,
    )?;
    if !closure.is_complete() {
        bail!("qualification consumer retained source/content closure is incomplete");
    }
    Ok(())
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
            | ProductSelectionTarget::ExecutionDependency { .. }
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

/// Existing verifier identity owner, reused by the activated-content source
/// adapter. No product witness or selection is synthesized for a fixed pin.
/// This returns definition coordinates only; accepted-root admission must
/// independently compare them and seal the complete qualification purpose.
pub(crate) fn resolve_content_fixed_pin_verifier(
    state: &AppState,
    context: &HandlerContext,
    policy: &ProductQualificationPolicySource,
    subject: &ryeos_state::external_content::qualification_subject::ContentQualificationSubject,
) -> anyhow::Result<(String, String)> {
    policy.validate()?;
    subject.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let current = resolve_current_bundle_verifier_identity_against_admitted(
        state,
        &authority,
        &guard,
        context,
        &policy.policy.verifier_ref,
        &policy.policy.verifier_parameters,
        CurrentVerifierContext {
            content: CurrentVerifierContent::Root(None),
            logical_project_root: None,
            binding_subject_authority: None,
            sealed_request: None,
            project_context_resolver: None,
            pinned_admission: None,
        },
        None,
    )?;
    subject.verify_verifier_realizations(
        &current.realizations,
        &policy.policy.subject_declaration_id,
    )?;
    Ok((
        current.admitted_definition_digest,
        current.effective_definition_digest,
    ))
}

/// Resolve the current signed policy in the deliberately narrow projectless
/// Bundle lane. Project Config policy needs an exact retained context owner and
/// is refused here rather than falling back to live source.
pub(super) fn resolve_current_bundle_qualification_policy(
    state: &AppState,
    policy_ref: &str,
) -> anyhow::Result<ProductQualificationPolicySource> {
    state.engine.with_checked_bundle_generation(|generation| {
        resolve_current_bundle_qualification_policy_in_generation(generation, policy_ref)
    })
}

pub(crate) fn resolve_current_bundle_qualification_policy_in_generation(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    policy_ref: &str,
) -> anyhow::Result<ProductQualificationPolicySource> {
    let resolution = resolve_consumer_definition_in_generation(generation, policy_ref, "config")?;
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

/// Read the three signed consumer definitions under one checked Bundle
/// generation. This is definition identity only; Worker D0 must be derived
/// after source admission and before product selection. Neither that source
/// closure nor runtime behavior is proved here.
pub(super) fn resolve_current_bundle_consumer_definitions(
    state: &AppState,
    policy_source: &ProductQualificationPolicySource,
    relationship: &ryeos_state::external_content::products::composition::ProductRelationship,
) -> anyhow::Result<Option<ProductQualificationConsumerDefinitionIdentity>> {
    let Some(context) = &policy_source.policy.consumer_execution_context else {
        return Ok(None);
    };
    context.validate_relationship_consumer(&relationship.consumer)?;
    state.engine.with_checked_bundle_generation(|generation| {
        let current_policy = resolve_current_bundle_qualification_policy_in_generation(
            generation,
            &policy_source.canonical_ref,
        )?;
        if current_policy != *policy_source {
            bail!("qualification consumer policy changed during definition admission");
        }
        let worker =
            resolve_consumer_definition_in_generation(generation, &context.worker_ref, "worker")?;
        let environment = resolve_consumer_definition_in_generation(
            generation,
            &context.environment_ref,
            "config",
        )?;
        let worker_execution = resolve_consumer_definition_in_generation(
            generation,
            &context.worker_execution_ref,
            "worker_execution",
        )?;
        let definitions = ProductQualificationConsumerDefinitionIdentity {
            bundle_generation_identity: generation.request_engine_generation_identity().into(),
            worker: consumer_definition_identity(&worker)?,
            environment: consumer_definition_identity(&environment)?,
            worker_execution: consumer_definition_identity(&worker_execution)?,
        };
        definitions.validate_for(context)?;
        Ok(Some(definitions))
    })
}

/// Admit the exact signed consumer Worker source against the policy's
/// same-generation definition coordinates. This binds the source-derived D0
/// but deliberately does not attest the environment, product or runtime.
/// Component tests use this standalone path; production stages the full closure.
#[cfg(test)]
pub(super) fn admit_current_bundle_consumer_worker(
    state: &AppState,
    policy_source: &ProductQualificationPolicySource,
    relationship: &ryeos_state::external_content::products::composition::ProductRelationship,
) -> anyhow::Result<
    Option<(
        ProductQualificationConsumerDefinitionIdentity,
        crate::source_closure_admission::AdmittedBundleStructuredWorkerProfile,
    )>,
> {
    let Some(definitions) =
        resolve_current_bundle_consumer_definitions(state, policy_source, relationship)?
    else {
        return Ok(None);
    };
    let prepared = crate::source_closure_admission::prepare_bundle_structured_worker_profile(
        state,
        &definitions.worker.canonical_ref,
    )?;
    let worker = prepared.admitted();
    require_consumer_worker_matches_definitions(&definitions, worker)?;
    Ok(Some((definitions, prepared.publish()?)))
}

fn require_consumer_worker_matches_definitions(
    definitions: &ProductQualificationConsumerDefinitionIdentity,
    worker: &crate::source_closure_admission::AdmittedBundleStructuredWorkerProfile,
) -> anyhow::Result<()> {
    if definitions.bundle_generation_identity != worker.bundle_generation_identity
        || definitions.worker.raw_content_digest != worker.raw_content_digest
        || definitions.worker.publisher_fingerprint != worker.publisher_fingerprint
        || definitions.worker.effective_definition_digest
            != worker.signed_effective_definition_digest
    {
        bail!("qualification consumer Worker source differs from signed definitions");
    }
    Ok(())
}

/// Signed environment intent from the same consumer-definition generation.
/// These are declarations, not captured/verified realizations or applied
/// process settings. The admission owner must still resolve exact content.
pub(super) struct CurrentBundleConsumerEnvironmentDefinition {
    pub definitions: ProductQualificationConsumerDefinitionIdentity,
    pub declarations: Vec<ryeos_engine::external_content::ExternalContentDeclaration>,
    pub executable_search: Vec<ExecutableSearchPathEntry>,
    pub process_environment: BTreeMap<String, SessionProcessEnvironmentValue>,
}

/// Exact admitted environment content. This still says nothing about guest
/// delivery or the verifier's observed process environment.
pub(super) struct AdmittedBundleConsumerEnvironment {
    pub definition: CurrentBundleConsumerEnvironmentDefinition,
    pub realizations: ExternalContentRealizationSet,
    pub realized_effective_definition_digest: String,
}

/// Standalone stage for component tests; production stages Worker and environment
/// together.
#[cfg(test)]
pub(super) struct PreparedBundleConsumerEnvironment {
    admitted: AdmittedBundleConsumerEnvironment,
    /// Keep the staged CAS roots alive while the test inspects admitted content.
    _publication: Option<ryeos_state::PendingCasPublication>,
}

/// Proof inputs for a direct verifier, not a runnable Worker. The source and
/// literal content share one staged CAS publication; the selected product
/// remains unselected, so ordinary Worker admission is still impossible here.
/// This preparatory value has no publication method: the future launch owner
/// must retain the complete stage under its exact verifier purpose before use.
pub(super) struct PreparedBundleConsumerWorkerLiterals {
    pub definitions: ProductQualificationConsumerDefinitionIdentity,
    pub relationship_definition: ProductQualificationBundleDefinitionIdentity,
    pub source: crate::source_closure_admission::AdmittedBundleStructuredWorkerProfile,
    pub literal_realizations: ExternalContentRealizationSet,
    publication: Option<ryeos_state::PendingCasPublication>,
}

/// The independently admitted content half of a direct consumer probe.
/// One pending stage protects source, Worker literals and environment bytes;
/// no product selection or qualification claim is created by this value.
pub(super) struct PreparedBundleConsumerContentInputs {
    pub definitions: ProductQualificationConsumerDefinitionIdentity,
    pub relationship_definition: ProductQualificationBundleDefinitionIdentity,
    pub policy_source: ProductQualificationPolicySource,
    pub worker_source: crate::source_closure_admission::AdmittedBundleStructuredWorkerProfile,
    pub worker_literals: ExternalContentRealizationSet,
    pub environment: AdmittedBundleConsumerEnvironment,
    runtime_member: Option<ProductQualificationConsumerRuntimeMemberIdentity>,
    publication: Option<ryeos_state::PendingCasPublication>,
}

impl PreparedBundleConsumerContentInputs {
    fn retained_identity(&self) -> anyhow::Result<ProductQualificationConsumerContentIdentity> {
        let context = self
            .policy_source
            .policy
            .consumer_execution_context
            .as_ref()
            .context("prepared consumer content has no signed context")?;
        let identity = ProductQualificationConsumerContentIdentity {
            definitions: self.definitions.clone(),
            relationship_definition: self.relationship_definition.clone(),
            worker_source: self.worker_source.source.clone(),
            worker_profile_hash: self.worker_source.profile.profile_hash.clone(),
            worker_preselection_effective_definition_digest: self
                .worker_source
                .preselection_effective_definition_digest
                .clone(),
            worker_literals: self.worker_literals.clone(),
            environment_realized_effective_definition_digest: self
                .environment
                .realized_effective_definition_digest
                .clone(),
            environment_realizations: self.environment.realizations.clone(),
            executable_search: self.environment.definition.executable_search.clone(),
            process_environment: self.environment.definition.process_environment.clone(),
            runtime_member: self
                .runtime_member
                .clone()
                .context("prepared consumer content has no exact runtime member join")?,
        };
        identity.validate_for(context, &self.definitions)?;
        Ok(identity)
    }

    fn into_publication(self) -> anyhow::Result<ryeos_state::PendingCasPublication> {
        self.publication
            .context("prepared consumer content has no staged CAS publication")
    }

    /// Align the direct probe with the signed external Worker route before
    /// verifier contact. This proves only executable-member identity, not
    /// hosted exec-server, placement, or candidate-lifecycle qualification.
    fn require_external_runtime_member_alignment(
        &mut self,
        state: &AppState,
        authority: &ryeos_state::PinnedStateAuthority,
        guard: &ryeos_state::CasMutationGuard,
        limits: ryeos_state::object_closure::ObjectClosureLimits,
        subject_manifest_hash: &str,
    ) -> anyhow::Result<()> {
        authority.ensure_guard(guard)?;
        let context = self
            .policy_source
            .policy
            .consumer_execution_context
            .as_ref()
            .context("external runtime alignment has no signed consumer context")?;
        let requirement = self
            .worker_source
            .profile
            .external_candidate_requirement()?
            .context("consumer Worker has no external candidate requirement")?;
        if requirement.runtime_product_declaration_id != context.product_declaration_id {
            bail!("external Worker runtime product differs from qualification subject");
        }
        if self.policy_source.policy.producer_scenarios.is_empty() {
            bail!("external runtime qualification has no signed direct probe");
        }
        let manifest = ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &authority.cas_store()?,
            subject_manifest_hash,
            limits
                .max_object_bytes
                .min(ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64),
        )?;
        let executable_hash = exact_runtime_member_hash(
            &manifest,
            &requirement.runtime_recipe.executable_relative_path,
        )?;
        state.engine.with_checked_bundle_generation(|generation| {
            if generation.request_engine_generation_identity()
                != self.definitions.bundle_generation_identity
            {
                bail!("external runtime alignment changed Bundle generation");
            }
            for scenario in self.policy_source.policy.producer_scenarios.values() {
                let recipe = resolve_bundle_producer_recipe_in_generation(
                    state,
                    generation.request_engine_generation_identity(),
                    &scenario.recipe_ref,
                )?;
                require_direct_consumer_target(
                    &self.policy_source,
                    &recipe,
                    subject_manifest_hash,
                )?;
                let ProducerExecutableSource::AdmittedRealizationMember {
                    relative_path,
                    executable_sha256,
                    ..
                } = &recipe.recipe.executable_source
                else {
                    bail!("external runtime probe does not target a retained product member");
                };
                if relative_path != &requirement.runtime_recipe.executable_relative_path
                    || executable_sha256 != &executable_hash
                {
                    bail!("external runtime probe differs from the Worker product executable");
                }
            }
            Ok(())
        })?;
        self.runtime_member = Some(ProductQualificationConsumerRuntimeMemberIdentity {
            product_declaration_id: context.product_declaration_id.clone(),
            relative_path: requirement.runtime_recipe.executable_relative_path.clone(),
            executable_sha256: executable_hash,
        });
        Ok(())
    }
}

#[cfg(test)]
impl PreparedBundleConsumerEnvironment {
    pub fn admitted(&self) -> &AdmittedBundleConsumerEnvironment {
        &self.admitted
    }
}

#[cfg(test)]
pub(super) fn prepare_current_bundle_consumer_environment(
    state: &AppState,
    policy_source: &ProductQualificationPolicySource,
    relationship: &ryeos_state::external_content::products::composition::ProductRelationship,
) -> anyhow::Result<Option<PreparedBundleConsumerEnvironment>> {
    let Some(definition) =
        resolve_current_bundle_consumer_environment_definition(state, policy_source, relationship)?
    else {
        return Ok(None);
    };
    let mut publication = None;
    let (realizations, realized_effective_definition_digest) =
        prepare_exact_bundle_consumer_realizations(
            state,
            &definition.definitions.bundle_generation_identity,
            &definition.definitions.environment,
            "config",
            &definition.declarations,
            &mut publication,
        )?;
    Ok(Some(PreparedBundleConsumerEnvironment {
        admitted: AdmittedBundleConsumerEnvironment {
            definition,
            realizations,
            realized_effective_definition_digest,
        },
        _publication: publication,
    }))
}

pub(super) fn prepare_current_bundle_consumer_content_inputs(
    state: &AppState,
    policy_source: &ProductQualificationPolicySource,
    relationship: &ryeos_state::external_content::products::composition::ProductRelationship,
    relationship_ref: &str,
) -> anyhow::Result<Option<PreparedBundleConsumerContentInputs>> {
    let Some(worker) = prepare_current_bundle_consumer_worker_literals(
        state,
        policy_source,
        relationship,
        relationship_ref,
    )?
    else {
        return Ok(None);
    };
    let PreparedBundleConsumerWorkerLiterals {
        definitions,
        relationship_definition,
        source,
        literal_realizations,
        mut publication,
    } = worker;
    let environment_definition =
        resolve_current_bundle_consumer_environment_definition(state, policy_source, relationship)?
            .context("qualification consumer has no signed environment definition")?;
    if environment_definition.definitions != definitions {
        bail!("qualification Worker and environment changed Bundle definitions");
    }
    let (realizations, realized_effective_definition_digest) =
        prepare_exact_bundle_consumer_realizations(
            state,
            &definitions.bundle_generation_identity,
            &definitions.environment,
            "config",
            &environment_definition.declarations,
            &mut publication,
        )?;
    Ok(Some(PreparedBundleConsumerContentInputs {
        definitions,
        relationship_definition,
        policy_source: policy_source.clone(),
        worker_source: source,
        worker_literals: literal_realizations,
        environment: AdmittedBundleConsumerEnvironment {
            definition: environment_definition,
            realizations,
            realized_effective_definition_digest,
        },
        runtime_member: None,
        publication,
    }))
}

/// Fresh selection must use today's signed consumer definitions and exact
/// content, not merely trust the historical verifier capsule. This check does
/// not grant a qualification claim; applied-runtime parity remains separate.
pub(in crate::operator_external_content) fn require_current_consumer_content_for_selection(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    policy_source: &ProductQualificationPolicySource,
    relationship: &ryeos_state::external_content::products::composition::ProductRelationship,
    evidence: &ProductQualificationEvidence,
    subject_manifest_hash: &str,
) -> anyhow::Result<()> {
    let retained = evidence
        .consumer_content
        .as_ref()
        .context("consumer qualification evidence has no retained content")?;
    require_retained_consumer_content_closure(authority, guard, limits, retained)?;
    let mut current = prepare_current_bundle_consumer_content_inputs(
        state,
        policy_source,
        relationship,
        &retained.relationship_definition.canonical_ref,
    )?
    .context("current signed consumer content is absent")?;
    current.require_external_runtime_member_alignment(
        state,
        authority,
        guard,
        limits,
        subject_manifest_hash,
    )?;
    if current.retained_identity()? != *retained {
        bail!("current consumer source, environment or runtime member changed");
    }
    Ok(())
}

pub(super) fn prepare_current_bundle_consumer_worker_literals(
    state: &AppState,
    policy_source: &ProductQualificationPolicySource,
    relationship: &ryeos_state::external_content::products::composition::ProductRelationship,
    relationship_ref: &str,
) -> anyhow::Result<Option<PreparedBundleConsumerWorkerLiterals>> {
    let Some(definitions) =
        resolve_current_bundle_consumer_definitions(state, policy_source, relationship)?
    else {
        return Ok(None);
    };
    let context = policy_source
        .policy
        .consumer_execution_context
        .as_ref()
        .context("qualification consumer context is absent")?;
    let source = crate::source_closure_admission::prepare_bundle_structured_worker_profile(
        state,
        &context.worker_ref,
    )?;
    require_consumer_worker_matches_definitions(&definitions, source.admitted())?;
    let (source, source_publication) = source.into_staged_parts();
    let mut publication = Some(
        source_publication.context("qualification Worker source has no staged CAS publication")?,
    );
    let (literal_realizations, relationship_definition) =
        state.engine.with_checked_bundle_generation(|generation| {
            if generation.request_engine_generation_identity()
                != definitions.bundle_generation_identity
            {
                bail!("qualification Worker changed Bundle generation before literal admission");
            }
            let mut resolution = resolve_consumer_definition_in_generation(
                generation,
                &context.worker_ref,
                "worker",
            )?;
            if consumer_definition_identity(&resolution)? != definitions.worker {
                bail!("qualification Worker changed signed definition before literal admission");
            }
            let relationship_definition =
                resolve_consumer_definition_in_generation(generation, relationship_ref, "config")?;
            let relationship_identity = consumer_definition_identity(&relationship_definition)?;
            let signed_relationships =
            ryeos_state::external_content::products::composition::ProductRelationships::from_value(
                relationship_definition
                    .composed
                    .composed
                    .get("product_relationships")
                    .context("qualification product relationship Config has no relationships")?
                    .clone(),
            )?;
            if signed_relationships.select(&relationship.name)? != relationship {
                bail!("qualification relationship differs from its current signed Config");
            }
            let contract = state
                .engine
                .kinds
                .get("worker")
                .and_then(|schema| schema.external_content_contract())
                .context("qualification Worker has no signed content contract")?;
            let declarer = ryeos_engine::external_content::declaring_authority(&resolution)?;
            let shape = ryeos_engine::external_content::authored_external_content_shape(
                &resolution.composed.composed,
                Some(contract),
                declarer,
            )?
            .context("qualification Worker has no signed content shape")?;
            if shape.product_slots.len() != 1
                || shape.product_slots[0].id != context.product_declaration_id
                || shape.product_slots[0].relationship != relationship.name
                || shape.product_slots[0].relationship_ref != relationship_ref
            {
                bail!("qualification Worker product slot differs from signed relationship");
            }
            resolution
                .composed
                .derived
                .insert(SOURCE_CLOSURE_DERIVED_KEY.into(), source.source.to_value()?);
            crate::effective_program_preparation::prepare_hookless_preselection_effective_program(
                &state.engine,
                "worker",
                &mut resolution,
            )?;
            if resolution.effective_definition_digest()?.as_str()
                != source.preselection_effective_definition_digest
            {
                bail!("qualification Worker source-derived D0 changed before literal admission");
            }
            let roots = state.engine.resolution_roots(None);
            let (_admitted, declarations) = crate::external_content_admission::
                admit_pending_consumer_literal_realizations_in_publication(
                    state,
                    &state.engine,
                    "worker",
                    &mut resolution,
                    &roots,
                    &mut publication,
                )?;
            let literals = ExternalContentRealizationSet::from_value(
                resolution
                    .composed
                    .derived
                    .get(EXTERNAL_REALIZATIONS_DERIVED_KEY)
                    .context("qualification Worker omitted literal realization projection")?,
            )?;
            if literals.iter().len() != declarations.len()
                || declarations.iter().any(|declaration| {
                    !literals.iter().any(|realized| {
                        realized.id == declaration.id
                            && realized.kind == declaration.kind
                            && realized.mode == declaration.mode
                            && Some(realized.manifest_hash.as_str())
                                == declaration.digest.as_deref()
                            && realized.mount_root == declaration.mount_root
                            && realized.mount == declaration.mount
                    })
                })
            {
                bail!("qualification Worker literals differ from signed declarations");
            }
            Ok((literals, relationship_identity))
        })?;
    Ok(Some(PreparedBundleConsumerWorkerLiterals {
        definitions,
        relationship_definition,
        source,
        literal_realizations,
        publication,
    }))
}

fn prepare_exact_bundle_consumer_realizations(
    state: &AppState,
    bundle_generation_identity: &str,
    definition: &ProductQualificationBundleDefinitionIdentity,
    kind: &str,
    declarations: &[ryeos_engine::external_content::ExternalContentDeclaration],
    publication: &mut Option<ryeos_state::PendingCasPublication>,
) -> anyhow::Result<(ExternalContentRealizationSet, String)> {
    state.engine.with_checked_bundle_generation(|generation| {
        if generation.request_engine_generation_identity() != bundle_generation_identity {
            bail!("qualification consumer changed Bundle generation before realization");
        }
        let mut resolution =
            resolve_consumer_definition_in_generation(generation, &definition.canonical_ref, kind)?;
        if consumer_definition_identity(&resolution)? != *definition {
            bail!("qualification consumer changed signed definition before realization");
        }
        let roots = state.engine.resolution_roots(None);
        let _admitted =
            crate::external_content_admission::admit_external_realizations_in_publication(
                state,
                &state.engine,
                kind,
                &mut resolution,
                &roots,
                &SubjectResolutionAuthority::Projectless,
                None,
                publication,
            )?
            .context("qualification consumer produced no external realization")?;
        let realizations = ExternalContentRealizationSet::from_value(
            resolution
                .composed
                .derived
                .get(EXTERNAL_REALIZATIONS_DERIVED_KEY)
                .context("qualification consumer omitted exact realization projection")?,
        )?;
        if realizations.iter().len() != declarations.len() {
            bail!("qualification consumer realization count differs from signed content");
        }
        for declaration in declarations {
            let realized = realizations
                .iter()
                .find(|realized| realized.id == declaration.id)
                .context("qualification consumer realization is absent")?;
            if realized.kind != declaration.kind
                || realized.mode != declaration.mode
                || Some(realized.manifest_hash.as_str()) != declaration.digest.as_deref()
                || realized.mount_root != declaration.mount_root
                || realized.mount != declaration.mount
            {
                bail!("qualification consumer realization differs from signed declaration");
            }
        }
        Ok((
            realizations,
            resolution
                .effective_definition_digest()?
                .as_str()
                .to_owned(),
        ))
    })
}

pub(super) fn resolve_current_bundle_consumer_environment_definition(
    state: &AppState,
    policy_source: &ProductQualificationPolicySource,
    relationship: &ryeos_state::external_content::products::composition::ProductRelationship,
) -> anyhow::Result<Option<CurrentBundleConsumerEnvironmentDefinition>> {
    let Some(definitions) =
        resolve_current_bundle_consumer_definitions(state, policy_source, relationship)?
    else {
        return Ok(None);
    };
    let context = policy_source
        .policy
        .consumer_execution_context
        .as_ref()
        .context("qualification consumer context is absent")?;
    let (declarations, executable_search, process_environment) =
        state.engine.with_checked_bundle_generation(|generation| {
            if generation.request_engine_generation_identity()
                != definitions.bundle_generation_identity
            {
                bail!("qualification environment changed Bundle generation");
            }
            let environment = resolve_consumer_definition_in_generation(
                generation,
                &context.environment_ref,
                "config",
            )?;
            let worker_execution = resolve_consumer_definition_in_generation(
                generation,
                &context.worker_execution_ref,
                "worker_execution",
            )?;
            if consumer_definition_identity(&environment)? != definitions.environment
                || consumer_definition_identity(&worker_execution)? != definitions.worker_execution
            {
                bail!("qualification environment definitions changed after admission");
            }
            let authored = &environment.composed.composed;
            if authored
                .get("worker_ref")
                .and_then(serde_json::Value::as_str)
                != Some(context.worker_ref.as_str())
            {
                bail!("qualification environment names a different Worker");
            }
            let configuration = authored
                .get("configuration")
                .context("qualification environment has no configuration")?;
            let executable_search: Vec<ExecutableSearchPathEntry> = serde_json::from_value(
                configuration
                    .get("executable_search")
                    .cloned()
                    .context("qualification environment has no executable search")?,
            )?;
            let process_environment: BTreeMap<String, SessionProcessEnvironmentValue> =
                serde_json::from_value(
                    configuration
                        .get("process_environment")
                        .cloned()
                        .context("qualification environment has no process environment")?,
                )?;
            if executable_search.len() > ryeos_state::objects::MAX_EXECUTABLE_SEARCH_PATH_ENTRIES {
                bail!("qualification environment executable search exceeds bound");
            }
            let mut search_ids = std::collections::BTreeSet::new();
            for entry in &executable_search {
                entry.validate()?;
                if !search_ids.insert((&entry.realization_id, &entry.relative_directory)) {
                    bail!("qualification environment has duplicate executable search");
                }
            }
            ryeos_state::objects::validate_session_process_environment(&process_environment)?;
            let execution_config = worker_execution
                .composed
                .composed
                .get("config")
                .context("qualification WorkerExecution has no config")?;
            if execution_config
                .get("environment_binding")
                .and_then(serde_json::Value::as_str)
                != Some(context.environment_binding.as_str())
            {
                bail!("qualification WorkerExecution environment binding differs from policy");
            }
            let execution_worker = execution_config
                .get("worker_ref")
                .context("qualification WorkerExecution has no worker_ref")?;
            if !execution_worker.is_null()
                && execution_worker.as_str() != Some(context.worker_ref.as_str())
            {
                bail!("qualification WorkerExecution names a different Worker");
            }
            let contract = state
                .engine
                .kinds
                .get("config")
                .and_then(|schema| schema.external_content_contract());
            let declarer = ryeos_engine::external_content::declaring_authority(&environment)?;
            let declarations =
                ryeos_engine::external_content::effective_external_content_declarations(
                    &environment,
                    contract,
                    declarer,
                )?
                .context("qualification environment has no external-content declaration")?;
            if declarations.is_empty()
                || declarations.iter().any(|declaration| {
                    declaration.mode != ryeos_engine::external_content::ExternalContentMode::Pinned
                        || declaration.locator.is_some()
                        || declaration.digest.is_none()
                })
            {
                bail!("qualification environment requires exact pinned tree declarations");
            }
            for entry in &executable_search {
                if !declarations.iter().any(|declaration| {
                    declaration.id == entry.realization_id
                        && declaration.kind
                            == ryeos_engine::external_content::ExternalContentKind::Tree
                }) {
                    bail!("qualification executable search names absent or non-tree content");
                }
            }
            for value in process_environment.values() {
                let SessionProcessEnvironmentValue::RealizationPath { realization_id, .. } = value
                else {
                    continue;
                };
                if !declarations.iter().any(|declaration| {
                    declaration.id == *realization_id
                        && declaration.kind
                            == ryeos_engine::external_content::ExternalContentKind::Tree
                }) {
                    bail!("qualification process environment names absent or non-tree content");
                }
            }
            Ok((declarations, executable_search, process_environment))
        })?;
    Ok(Some(CurrentBundleConsumerEnvironmentDefinition {
        definitions,
        declarations,
        executable_search,
        process_environment,
    }))
}

fn resolve_consumer_definition_in_generation(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    item_ref: &str,
    kind: &str,
) -> anyhow::Result<ResolutionOutput> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)?;
    if canonical.to_string() != item_ref || canonical.suffix.is_some() || canonical.kind != kind {
        bail!("qualification consumer definition must be an exact {kind} Bundle ref");
    }
    let resolution =
        generation.effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: canonical,
            expected_kind: Some(kind.into()),
            project_root: None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
        })?;
    if resolution.root.resolved_ref != item_ref
        || resolution.effective_trust_class != TrustClass::TrustedBundle
        || resolution.root.source_space != ItemSpace::Bundle
        || !matches!(resolution.root.source_root, ItemSourceRoot::Bundle { .. })
        || resolution.root.signer_fingerprint.is_none()
    {
        bail!("qualification consumer definition must resolve from a trusted Bundle");
    }
    Ok(resolution)
}

fn consumer_definition_identity(
    resolution: &ResolutionOutput,
) -> anyhow::Result<ProductQualificationBundleDefinitionIdentity> {
    let identity = ProductQualificationBundleDefinitionIdentity {
        canonical_ref: resolution.root.resolved_ref.clone(),
        raw_content_digest: resolution.root.raw_content_digest.clone(),
        effective_definition_digest: resolution.effective_definition_digest()?.as_str().into(),
        publisher_fingerprint: resolution
            .root
            .signer_fingerprint
            .clone()
            .context("consumer Bundle definition has no publisher")?,
    };
    identity.validate()?;
    Ok(identity)
}

/// Re-resolve a finite producer scenario from the exact current signed Bundle
/// generation. The callback supplies only `scenario_id`; the sealed purpose
/// selects the policy and that policy selects the Config recipe. This is a
/// preflight identity, not a process-launch grant. Callers must retain its
/// identity in the attempt journal and recheck it at the irreversible release.
pub fn resolve_current_bundle_producer_recipe_for_purpose(
    state: &AppState,
    purpose: &ryeos_state::external_content::qualification_execution::QualificationExecutionPurposeView<'_>,
    scenario_id: &str,
) -> anyhow::Result<CurrentBundleProducerRecipe> {
    state.engine.with_checked_bundle_generation(|generation| {
        let current_policy = resolve_current_bundle_qualification_policy(
            state,
            &purpose.policy_source().canonical_ref,
        )?;
        if &current_policy != purpose.policy_source() {
            bail!("qualification producer policy changed after root admission");
        }
        let scenario = current_policy
            .policy
            .producer_scenarios
            .get(scenario_id)
            .context("qualification producer scenario is not signed in the admitted policy")?;
        let current = resolve_bundle_producer_recipe_in_generation(
            state,
            generation.request_engine_generation_identity(),
            &scenario.recipe_ref,
        )?;
        require_direct_consumer_target(&current_policy, &current, purpose.subject_manifest_hash())?;
        if let Some(content) = purpose.consumer_content() {
            if generation.request_engine_generation_identity()
                != content.definitions.bundle_generation_identity
            {
                bail!("qualification consumer Bundle generation changed before direct probe");
            }
            let ProducerExecutableSource::AdmittedRealizationMember {
                relative_path,
                executable_sha256,
                ..
            } = &current.recipe.executable_source
            else {
                bail!("qualification direct probe no longer selects a product member");
            };
            if relative_path != &content.runtime_member.relative_path
                || executable_sha256 != &content.runtime_member.executable_sha256
            {
                bail!("qualification direct probe differs from retained consumer runtime member");
            }
        }
        state
            .node_policy
            .require::<NodeExecutionAdmissionPolicy>()?
            .admit_producer_bounds(&current.recipe.bounds)?;
        let admitted = purpose
            .producer_recipe_sources()
            .get(scenario_id)
            .context("qualification purpose did not admit the selected producer scenario")?;
        if current.source_identity()? != *admitted {
            bail!("qualification producer recipe changed after root admission");
        }
        Ok(current)
    })
}

/// Borrow the producer's input exclusively from the accepted verifier root.
/// Operational SQLite coordinates locate the CAS capsule, but neither caller
/// text nor a fresh policy resolution can supply or modify these bytes.
pub fn admitted_root_producer_stdin(
    state: &AppState,
    root_thread_id: &str,
    purpose: &ryeos_state::external_content::qualification_execution::QualificationExecutionPurposeView<'_>,
) -> anyhow::Result<String> {
    let (chain_root_id, _, capsule) = state
        .state_store
        .admitted_launch_capsule_with_coordinates(root_thread_id)?
        .context("qualification root has no admitted launch capsule")?;
    if chain_root_id != root_thread_id {
        bail!("producer input requires the exact accepted verifier root");
    }
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
        &capsule,
    )?;
    sealed.admitted_qualification_producer_stdin(purpose)
}

/// Called at the accepted-root cut, before purpose sealing or verifier spawn.
/// Resolve every finite scenario under one checked Bundle generation because
/// the verifier chooses its scenario only after root admission.
pub fn resolve_current_bundle_producer_recipes_for_policy(
    state: &AppState,
    policy_source: &ProductQualificationPolicySource,
    subject_manifest_hash: &str,
) -> anyhow::Result<BTreeMap<String, ProductProducerRecipeSourceIdentity>> {
    policy_source.validate()?;
    require_canonical_hash("qualification subject manifest", subject_manifest_hash)?;
    state.engine.with_checked_bundle_generation(|generation| {
        let current =
            resolve_current_bundle_qualification_policy(state, &policy_source.canonical_ref)?;
        if current != *policy_source {
            bail!("qualification producer policy changed before root admission");
        }
        let mut sources = BTreeMap::new();
        for (scenario_id, scenario) in &current.policy.producer_scenarios {
            let recipe = resolve_bundle_producer_recipe_in_generation(
                state,
                generation.request_engine_generation_identity(),
                &scenario.recipe_ref,
            )?;
            require_direct_consumer_target(&current, &recipe, subject_manifest_hash)?;
            state
                .node_policy
                .require::<NodeExecutionAdmissionPolicy>()?
                .admit_producer_bounds(&recipe.recipe.bounds)?;
            sources.insert(scenario_id.clone(), recipe.source_identity()?);
        }
        Ok(sources)
    })
}

/// A qualification that promises a consumer runtime must execute the signed
/// subject member itself. Running the verifier as the scoped target can still
/// qualify verifier-only policies, but cannot establish consumer parity.
fn require_direct_consumer_target(
    policy: &ProductQualificationPolicySource,
    recipe: &CurrentBundleProducerRecipe,
    subject_manifest_hash: &str,
) -> anyhow::Result<()> {
    if policy.policy.consumer_execution_context.is_some() {
        match &recipe.recipe.executable_source {
            ProducerExecutableSource::AdmittedRealizationMember {
                realization_id,
                manifest_hash,
                ..
            } if realization_id == &policy.policy.subject_declaration_id
                && manifest_hash == subject_manifest_hash => {}
            _ => bail!(
                "consumer runtime qualification requires the exact admitted subject executable"
            ),
        }
    }
    Ok(())
}

fn resolve_bundle_producer_recipe_in_generation(
    state: &AppState,
    generation_identity: &str,
    recipe_ref: &str,
) -> anyhow::Result<CurrentBundleProducerRecipe> {
    let resolution = resolve_current_trusted_bundle(state, recipe_ref, Some("config"))?;
    let recipe = ryeos_state::external_content::products::producer_recipe::ProductProducerRecipe::from_value(
        resolution
            .composed
            .composed
            .get(PRODUCER_RECIPE_FIELD)
            .context("producer Config has no product_producer_recipe")?
            .clone(),
    )?;
    Ok(CurrentBundleProducerRecipe {
        bundle_generation_identity: generation_identity.to_owned(),
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
            .context("trusted producer recipe has no publisher")?,
        recipe,
    })
}

/// The full signed source coordinate accompanies the parsed launch contract;
/// a recipe digest alone cannot distinguish source or bundle-generation drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentBundleProducerRecipe {
    pub bundle_generation_identity: String,
    pub canonical_ref: String,
    pub raw_content_digest: String,
    pub effective_definition_digest: String,
    pub publisher_fingerprint: String,
    pub recipe: ryeos_state::external_content::products::producer_recipe::ProductProducerRecipe,
}

impl CurrentBundleProducerRecipe {
    pub fn source_identity(&self) -> anyhow::Result<ProductProducerRecipeSourceIdentity> {
        let source = ProductProducerRecipeSourceIdentity {
            bundle_generation_identity: self.bundle_generation_identity.clone(),
            canonical_ref: self.canonical_ref.clone(),
            raw_content_digest: self.raw_content_digest.clone(),
            effective_definition_digest: self.effective_definition_digest.clone(),
            publisher_fingerprint: self.publisher_fingerprint.clone(),
            recipe_digest: self.recipe.digest()?,
        };
        source.validate()?;
        Ok(source)
    }
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
    project_context_resolver: Option<&dyn QualificationProjectContextResolver>,
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
    let purpose = sealed
        .qualification_purpose()
        .context("qualification evidence verifier has no sealed purpose")?;
    let (witness_hash, witness_source, _) = purpose.subject.captured_product()?;
    if purpose.consumer_content != evidence.consumer_content
        || witness_hash != evidence.product_witness_hash
        || witness_source != &evidence.witness_source
        || purpose.policy_source != evidence.policy_source
    {
        bail!("qualification evidence differs from sealed verifier purpose");
    }
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
            sealed_request: Some(&sealed),
            project_context_resolver,
            pinned_admission: None,
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
    let product_selections = current_root_selection_inputs(verifier_root_selections)?;
    let mut project_context_lease: Option<Box<dyn QualificationProjectContextLease>> = None;
    let (request_engine, plan_context, project_binding, project_root) = if let Some(sealed) =
        verifier_context.sealed_request
    {
        if sealed.item_ref() != verifier_ref {
            bail!("qualification verifier ref differs from its sealed launch request");
        }
        if sealed.admitted_parameters_digest()?
            != ryeos_state::objects::canonical_value_digest(verifier_parameters)?
        {
            bail!("qualification verifier parameters differ from its sealed launch request");
        }
        let exact_authority = sealed.project_authority().clone();
        exact_authority.validate()?;
        match &exact_authority {
            ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => {
                let plan_context = sealed.qualification_plan_context(None)?;
                let EffectivePrincipal::Local(principal) = &plan_context.requested_by else {
                    bail!(
                        "independent product qualification rejects delegated verifier principals"
                    );
                };
                context.validate_execution_authority(
                    &principal.fingerprint,
                    &principal.scopes,
                    &plan_context.current_site_id,
                    &plan_context.origin_site_id,
                )?;
                if plan_context.current_site_id != state.threads.site_id() {
                    bail!("projectless verifier current site differs from the serving node");
                }
                let engine = Arc::clone(&state.engine);
                let binding =
                    crate::thread_lifecycle::AdmittedProjectBinding::explicit_projectless(
                        &engine,
                        &plan_context,
                    )?;
                (engine, plan_context, binding, None)
            }
            ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                display_path: Some(display_path),
                snapshot_hash,
                realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
                ..
            } => {
                let resolver = verifier_context
                    .project_context_resolver
                    .context("pinned qualification verifier has no snapshot resolver")?;
                let lease = resolver.resolve_read_only_snapshot(
                    snapshot_hash,
                    display_path,
                    &format!("qualification-verifier-{}", uuid::Uuid::new_v4()),
                )?;
                if lease.snapshot_hash() != snapshot_hash
                    || lease.original_path() != display_path
                    || lease.pinned_materialization().snapshot_hash() != snapshot_hash
                    || lease.pinned_materialization().path() != lease.effective_path()
                    || lease.workspace_lifeline().path().is_none()
                    || !lease
                        .workspace_lifeline()
                        .owns_effective_path(lease.effective_path())
                    || !lease
                        .pinned_materialization()
                        .owns_path(lease.effective_path())?
                {
                    bail!("qualification snapshot resolver returned a contradictory lease");
                }
                lease.pinned_materialization().ensure_path_binding()?;
                let engine = Arc::clone(lease.request_engine());
                let plan_context =
                    sealed.qualification_plan_context(Some(lease.effective_path()))?;
                let EffectivePrincipal::Local(principal) = &plan_context.requested_by else {
                    bail!(
                        "independent product qualification rejects delegated verifier principals"
                    );
                };
                context.validate_execution_authority(
                    &principal.fingerprint,
                    &principal.scopes,
                    &plan_context.current_site_id,
                    &plan_context.origin_site_id,
                )?;
                if plan_context.current_site_id != state.threads.site_id() {
                    bail!("pinned verifier current site differs from the serving node");
                }
                let binding =
                    crate::thread_lifecycle::AdmittedProjectBinding::from_qualification_snapshot(
                        &engine,
                        &plan_context,
                        exact_authority.clone(),
                        lease.original_path(),
                        lease.effective_path(),
                        lease.pinned_materialization(),
                        Arc::clone(lease.workspace_lifeline()),
                    )?;
                let project_root = lease.effective_path().to_path_buf();
                project_context_lease = Some(lease);
                (engine, plan_context, binding, Some(project_root))
            }
            _ => bail!(
                "qualification verifier project authority is not projectless or read-only pinned"
            ),
        }
    } else if let Some(pinned) = verifier_context.pinned_admission {
        use ryeos_state::objects::{ExecutionProjectAuthority, PinnedProjectRealization};
        let ExecutionProjectAuthority::PinnedGeneration {
            snapshot_hash,
            realization: PinnedProjectRealization::ReadOnly,
            workspace_outputs: None,
            ..
        } = pinned.project_authority
        else {
            bail!("qualification selected-slot admission is not read-only pinned authority");
        };
        if !matches!(
            &pinned.plan_context.project_context,
            ProjectContext::SnapshotHash { hash } if hash == snapshot_hash
        ) || pinned.plan_context.subject_resolution_authority
            != (SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: snapshot_hash.clone(),
            })
        {
            bail!("qualification selected-slot admission snapshot authority differs");
        }
        if pinned.project_binding.exact_authority() != pinned.project_authority
            || pinned.project_binding.subject_resolution_authority()
                != &pinned.plan_context.subject_resolution_authority
        {
            bail!("qualification pinned project binding differs from its exact plan context");
        }
        let EffectivePrincipal::Local(principal) = &pinned.plan_context.requested_by else {
            bail!("independent product qualification rejects delegated verifier principals");
        };
        context.validate_execution_authority(
            &principal.fingerprint,
            &principal.scopes,
            &pinned.plan_context.current_site_id,
            &pinned.plan_context.origin_site_id,
        )?;
        if pinned.plan_context.current_site_id != state.threads.site_id() {
            bail!("pinned verifier current site differs from the serving node");
        }
        (
            Arc::clone(pinned.request_engine),
            pinned.plan_context.clone(),
            pinned.project_binding.clone(),
            Some(pinned.project_root.to_path_buf()),
        )
    } else {
        // Legacy in-memory callers are retained for projectless unit
        // fixtures only. Production evidence paths always carry a sealed
        // capsule and cannot invent project authority here.
        let plan_context = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: context.fingerprint.clone(),
                scopes: context.scopes.clone(),
            }),
            project_context: ProjectContext::None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            current_site_id: state.threads.site_id().to_owned(),
            origin_site_id: context.execution_origin(state.threads.site_id()),
            execution_hints: ExecutionHints::default(),
            scheduled_fire: None,
            validate_only: false,
        };
        let engine = Arc::clone(&state.engine);
        let binding = crate::thread_lifecycle::AdmittedProjectBinding::explicit_projectless(
            &engine,
            &plan_context,
        )?;
        (engine, plan_context, binding, None)
    };
    let projectless_binding_authority = SubjectResolutionAuthority::Projectless;
    let admitted_binding_authority = verifier_context
        .pinned_admission
        .map(|pinned| &pinned.plan_context.subject_resolution_authority)
        .unwrap_or(&projectless_binding_authority);
    let binding_subject_authority = current_verifier_binding_subject_authority(
        verifier_context.binding_subject_authority,
        admitted_binding_authority,
    );
    if let Some(sealed) = verifier_context.sealed_request
        && verifier_context.binding_subject_authority != Some(sealed.resolution_subject_authority())
    {
        bail!("qualification binding subject authority differs from its sealed verifier capsule");
    }
    let roots = request_engine.resolution_roots(project_root.clone());
    // Admit the exact current subject before choosing its execution route.
    // A signed delegated runtime need not declare a root executor chain.
    let preflight = crate::thread_lifecycle::preflight_root_execution(
        crate::thread_lifecycle::ResolveRootExecutionParams {
            engine: &request_engine,
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
    let admitted_definition_digest = admission
        .resolution_output()
        .effective_definition_digest()?
        .as_str()
        .to_owned();
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
    let contract = request_engine
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
    let staged_roots = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(guard, "qualification-verifier-recheck")?;
    let mut publication = Some(ryeos_state::PendingCasPublication::new(
        authority.try_clone()?,
        staged_roots,
    ));
    let source_contract = request_engine
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
            &request_engine.kinds,
            &request_engine.parser_dispatcher,
            &roots,
            &request_engine.trust_store,
            &request_engine.node_trust_store,
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
        &request_engine,
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
            &request_engine,
            &verified.resolved,
            &mut resolution,
            &roots,
            &request_engine.trust_store,
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
            &request_engine,
            &kind,
            &mut resolution,
            &roots,
            binding_subject_authority,
            inherited,
            &mut publication,
        )?
        .context("qualification verifier produced no current external realization")?,
    };
    let semantic_projection =
        ryeos_engine::effective_program::take_recovered_effective_program_derived(&mut resolution);
    let validation = request_engine
        .effective_validators
        .validate(&kind, &resolution)?;
    let candidate = ryeos_engine::effective_program::relock_recovered_effective_program(
        resolution,
        validation,
        semantic_projection,
    )?;
    let config_roots = request_engine.launch_config_roots(&roots);
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
    let execution = request_engine
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
                &request_engine,
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
                managed::resolve_current_managed_runtime_selection(&request_engine, None, &kind)?;
            let executor =
                managed::resolve_current_managed_executor_identity(&request_engine, &selection)?;
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
        admitted_definition_digest,
        effective_definition_digest,
        artifact_identity,
        request_engine,
        _project_context_lease: project_context_lease,
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
    load_current_qualification_with_key(
        authority,
        guard,
        limits,
        owner_principal,
        qualification_hash,
        state.identity.verifying_key(),
    )
}

pub(crate) fn load_current_qualification_with_key(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    owner_principal: &str,
    qualification_hash: &str,
    node_key: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<VerifiedQualificationWitness> {
    require_canonical_hash("qualification witness", qualification_hash)?;
    let value = load_product_attestation_value(authority, qualification_hash, limits, guard)?
        .context("qualification witness is absent")?;
    let attestation = Attestation::from_value(&value)?;
    let evidence = ProductQualificationEvidence::verify_attestation_for_owner(
        &attestation,
        node_key,
        owner_principal,
    )?;
    let coordinate = QualificationCoordinate::from_evidence(&evidence)?;
    let QualificationWitnessLookup::Found(witness) = lookup_qualification_witness_hash_guarded(
        authority,
        &coordinate,
        qualification_hash,
        node_key,
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

/// Reuse ordinary product-selection admission for a node-signed external
/// runtime binding. The binding names an owner and policy, but neither may
/// manufacture a verified request context: the current node-signed operator
/// grant supplies that authority. This is a fresh read-only check, not a
/// contact permit or retained recovery proof.
pub(crate) fn verify_current_external_runtime_qualification(
    state: &AppState,
    binding: &crate::node_config::sections::external_execution::ExternalRuntimeQualificationBinding,
    expected_manifest_hash: &str,
) -> anyhow::Result<VerifiedQualificationWitness> {
    binding.validate()?;
    require_canonical_hash("external runtime manifest", expected_manifest_hash)?;
    let operator = crate::operator_authority::admitted_operator_authority_for_principal(
        state,
        &binding.owner_principal,
    )?;
    let context = operator.handler_context();
    crate::operator_authority::require_admitted_operator(state, &context)?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let proof = load_current_qualification(
        state,
        &authority,
        &guard,
        limits,
        &binding.owner_principal,
        &binding.attestation_hash,
    )?;
    let product = super::product_receipt::load_product_source(
        state,
        &authority,
        &guard,
        limits,
        &binding.owner_principal,
        &proof.evidence.product_witness_hash,
        &proof.evidence.witness_source,
        super::product_receipt::ProductSourceVerification::Fresh,
    )?;
    if proof.evidence.product_witness_hash != product.attestation_hash
        || proof.evidence.product_coordinate
            != ProductCaptureCoordinate::from_evidence(&product.evidence)?
        || proof.evidence.result.subject_manifest_hash != product.evidence.manifest_hash
        || proof.evidence.result.subject_manifest_hash != expected_manifest_hash
    {
        bail!("external runtime qualification differs from its exact current product");
    }
    let policy_ref = binding
        .qualification
        .policy_ref
        .as_deref()
        .context("external runtime qualification has no signed policy")?;
    let current_policy = resolve_current_bundle_qualification_policy(state, policy_ref)?;
    if current_policy.policy.consumer_execution_context.is_some() {
        bail!("external runtime snapshot qualification cannot borrow a Worker consumer context");
    }
    let current_verifier = resolve_current_bundle_verifier_identity_for_evidence(
        state,
        &authority,
        &guard,
        limits,
        &context,
        &current_policy.policy.verifier_ref,
        &current_policy.policy.verifier_parameters,
        &proof.evidence,
        None,
    )?;
    proof.evidence.validate_current_policy(
        &current_policy,
        &current_verifier.effective_definition_digest,
        &binding.qualification.required_claims,
    )?;
    proof
        .evidence
        .validate_current_artifact(&current_verifier.artifact_identity)?;
    execution_evidence::verify_current(
        state,
        &authority,
        &guard,
        &context,
        &proof.evidence,
        &current_verifier,
        None,
    )?;
    Ok(proof)
}

/// Re-admit one retained qualification and return the exact current authority
/// pins an operator must review before enabling bundle publication. This is a
/// read-only measurement: it neither authors policy nor publishes CAS state.
pub fn measure_current_qualification_authority(
    state: &AppState,
    context: &HandlerContext,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    owner_principal: &str,
    qualification_hash: &str,
) -> anyhow::Result<MeasuredQualificationAuthority> {
    measure_current_qualification_authority_with_project_context_resolver(
        state,
        context,
        authority,
        guard,
        limits,
        owner_principal,
        qualification_hash,
        None,
    )
}

pub fn measure_current_qualification_authority_with_project_context_resolver(
    state: &AppState,
    context: &HandlerContext,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    owner_principal: &str,
    qualification_hash: &str,
    project_context_resolver: Option<&dyn QualificationProjectContextResolver>,
) -> anyhow::Result<MeasuredQualificationAuthority> {
    let proof = load_current_qualification(
        state,
        authority,
        guard,
        limits,
        owner_principal,
        qualification_hash,
    )?;
    let current_policy = resolve_current_bundle_qualification_policy(
        state,
        &proof.evidence.policy_source.canonical_ref,
    )?;
    let current_verifier = resolve_current_bundle_verifier_identity_for_evidence(
        state,
        authority,
        guard,
        limits,
        context,
        &current_policy.policy.verifier_ref,
        &current_policy.policy.verifier_parameters,
        &proof.evidence,
        project_context_resolver,
    )?;
    proof.evidence.validate_current_policy(
        &current_policy,
        &current_verifier.effective_definition_digest,
        &proof.evidence.result.claims,
    )?;
    proof
        .evidence
        .validate_current_artifact(&current_verifier.artifact_identity)?;
    Ok(MeasuredQualificationAuthority {
        qualified_product_witness_hash: proof.evidence.product_witness_hash.clone(),
        qualified_product_owner_principal: proof
            .evidence
            .product_coordinate
            .owner_principal
            .clone(),
        qualification_signer_public_key: state.identity.verifying_key().to_bytes(),
        qualification_signer_fingerprint: state.identity.fingerprint().to_owned(),
        qualification_policy: current_policy,
        qualification_verifier_effective_definition_digest: current_verifier
            .effective_definition_digest,
        qualification_verifier_artifact_identity: current_verifier.artifact_identity,
        required_qualification_claims: proof.evidence.result.claims.clone(),
    })
}

/// Recovery authenticates the exact retained proof under its caller's CAS
/// guard. It deliberately does not reapply current-head, current-policy, or
/// wall-clock eligibility.
pub(crate) fn verify_retained_qualification_guarded(
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
    if let Some(content) = &evidence.consumer_content {
        require_retained_consumer_content_closure(authority, guard, limits, content)?;
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
    fn direct_runtime_member_requires_exact_executable_file_in_both_manifest_tiers() {
        let executable = "a".repeat(64);
        for (kind, schema) in [
            (
                ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
                ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA,
            ),
            (
                ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
                ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA,
            ),
        ] {
            let mut manifest = serde_json::json!({
                "kind":kind,
                "schema":schema,
                "entry_count":2,
                "total_bytes":4,
                "entries":[
                    {"path":"bin","kind":"dir"},
                    {"path":"bin/codex","kind":"file","mode":493,
                     "blob_hash":executable,"size":4}
                ],
            });
            assert_eq!(
                super::exact_runtime_member_hash(&manifest, "bin/codex").unwrap(),
                executable
            );
            assert!(super::exact_runtime_member_hash(&manifest, "bin/other").is_err());
            manifest["entries"][1]["mode"] = serde_json::json!(420);
            assert!(super::exact_runtime_member_hash(&manifest, "bin/codex").is_err());
        }
    }

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

    #[test]
    fn verifier_preserves_auxiliary_root_product_selections() {
        let subject = root_selection();
        let mut auxiliary = root_selection();
        auxiliary.declaration_id = "tools".to_owned();
        auxiliary.relationship_name = "tools_to_verifier".to_owned();
        auxiliary.relationship.name = auxiliary.relationship_name.clone();
        auxiliary.relationship.consumer.declaration_id = auxiliary.declaration_id.clone();
        auxiliary.witness_hash = "b".repeat(64);
        auxiliary.declaration.id = auxiliary.declaration_id.clone();
        auxiliary.declaration.mount = "qualification/tools".to_owned();
        subject.validate().unwrap();
        auxiliary.validate().unwrap();
        let retained = ResolvedExternalProductSelections::new(BTreeMap::from([
            (subject.declaration_id.clone(), subject.clone()),
            (auxiliary.declaration_id.clone(), auxiliary.clone()),
        ]))
        .unwrap();
        let selector = |selected: &ResolvedExternalProductSelection| ProductSelectionInput {
            target: ProductSelectionTarget::Root {},
            selection: ProductSelection {
                declaration_id: selected.declaration_id.clone(),
                witness_hash: selected.witness_hash.clone(),
                witness_source: selected.witness_source.clone(),
                qualification_hash: None,
            },
        };
        let inputs = [selector(&subject), selector(&auxiliary)];
        let (selectors, recovered) =
            match_retained_verifier_root_selections(&inputs, Some(retained.clone()), "subject")
                .unwrap();
        assert_eq!(
            selectors,
            inputs
                .iter()
                .map(|input| input.selection.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(recovered, Some(retained.clone()));

        let mut mismatched = inputs;
        mismatched[1].selection.witness_hash = "c".repeat(64);
        assert!(
            match_retained_verifier_root_selections(&mismatched, Some(retained), "subject")
                .is_err()
        );
    }
}
