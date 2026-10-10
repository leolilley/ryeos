//! Controller-retained signed probe source for one original qualification root.
//! This captures definition provenance, not a native program or launch permit.

use anyhow::{Context as _, Result, ensure};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
    SubjectResolutionAuthority, VerifiedItem,
};
use ryeos_engine::engine::{CapturedSignedBundleItemSource, CheckedEngineGeneration};
use ryeos_engine::launch_config::LaunchConfigSnapshotSet;
use ryeos_engine::resolution::ResolutionOutput;
use ryeos_state::PendingCasPublication;
use ryeos_state::external_content::products::producer_recipe::{
    ProducerExecutableSource, ProducerStdinSource, ProductProducerRecipe,
};
use ryeos_state::external_content::products::qualification::ProductProducerRecipeSourceIdentity;
use ryeos_state::external_content::qualification_execution::QualificationExecutionPurposeView;
use serde_json::Value;
use std::collections::BTreeMap;

use super::CurrentBundleProducerRecipe;
use crate::scoped_producer_authority::{
    ScopedProducerAttemptCoordinate, ScopedProducerAuthorityKey,
};
use crate::state::AppState;

/// Only root preparation constructs this value from verified Bundle sources.
/// No callback, serialized plan, descriptor number or diagnostic path can
/// construct it. Actual native program/input admission remains a later owner.
pub struct CapturedMechanismProbeSource {
    root_key: ScopedProducerAuthorityKey,
    root_capsule_hash: String,
    scenario_id: String,
    recipe_source: ProductProducerRecipeSourceIdentity,
    verified: VerifiedItem,
    resolution: ResolutionOutput,
    parameters: BTreeMap<String, Value>,
    signed_sources: Vec<CapturedSignedBundleItemSource>,
    signed_bundle_manifests: Vec<ryeos_engine::plan_builder::CapturedSignedBundleManifest>,
}

/// Owner-retained preparation only. Every field remains coupled to the
/// original sealed root and staged CAS roots until native input preparation.
/// Holding this value does not authorize a helper launch or a qualification.
#[allow(dead_code)]
pub(crate) struct PreparedMechanismProbeProgram {
    root_key: ScopedProducerAuthorityKey,
    root_capsule_hash: String,
    scenario_id: String,
    recipe_source: ProductProducerRecipeSourceIdentity,
    coordinate: ScopedProducerAttemptCoordinate,
    deadline: lillux::time::MonotonicDeadline,
    finalized: ryeos_engine::effective_program::FinalizedEffectiveProgram,
    request: crate::thread_lifecycle::ResolvedExecutionRequest,
    protocol: ryeos_engine::protocols::VerifiedProtocol,
    closure: ryeos_state::objects::AdmittedExecutionClosure,
    artifact: ryeos_state::objects::AdmittedLaunchArtifactIdentity,
    plan: crate::thread_lifecycle::PreparedItemPlan,
    source: Option<crate::source_closure_admission::AdmittedSourceClosure>,
    external: crate::external_content_admission::AdmittedExternalRealizations,
    publication: PendingCasPublication,
    config_snapshots: Option<LaunchConfigSnapshotSet>,
}

/// Exact signed fixture bytes beside their original B program and staged CAS
/// owners. This is a private partial CAS, not project or execution authority.
#[allow(dead_code)]
pub struct PreparedMechanismFixtureTransfer {
    selected_snapshot_hash: String,
    measurement: ryeos_project_capture::ProjectSnapshotTransferMeasurement,
    descriptor: lillux::InheritedDescriptorAuthority,
    // Drop the private copy before the program's staged source/product roots.
    transfer: ryeos_project_capture::PreparedProjectSnapshotTransfer,
    program: PreparedMechanismProbeProgram,
}

impl CapturedMechanismProbeSource {
    fn selected_fixture_snapshot_hash(&self) -> Result<&str> {
        let fixture = self
            .parameters
            .get("mechanism_fixture")
            .and_then(Value::as_object)
            .context("mechanism probe has no strict signed fixture object")?;
        ensure!(
            fixture.len() == 1 && fixture.contains_key("project_snapshot_hash"),
            "mechanism fixture has unknown or missing fields"
        );
        let snapshot_hash = fixture
            .get("project_snapshot_hash")
            .and_then(Value::as_str)
            .context("mechanism fixture has no project snapshot hash")?;
        super::require_canonical_hash("mechanism fixture project snapshot", snapshot_hash)?;
        Ok(snapshot_hash)
    }

    pub fn require_root_key(&self, key: &ScopedProducerAuthorityKey) -> Result<()> {
        ensure!(
            &self.root_key == key,
            "mechanism probe source belongs to a different root owner"
        );
        Ok(())
    }

    pub fn require_source_identity(
        &self,
        scenario: &str,
        source: &ProductProducerRecipeSourceIdentity,
    ) -> Result<()> {
        ensure!(
            self.scenario_id == scenario && &self.recipe_source == source,
            "mechanism probe source differs from original signed scenario"
        );
        Ok(())
    }

    pub fn require_recipe(
        &self,
        scenario: &str,
        source: &ProductProducerRecipeSourceIdentity,
        recipe: &ProductProducerRecipe,
    ) -> Result<()> {
        let ProducerStdinSource::TrustedMechanismBootstrap {
            probe_ref,
            probe_parameters,
        } = &recipe.stdin_source
        else {
            anyhow::bail!("mechanism probe source requires trusted bootstrap selection");
        };
        ensure!(
            self.scenario_id == scenario
                && &self.recipe_source == source
                && self.resolution.root.resolved_ref == *probe_ref
                && &self.parameters == probe_parameters,
            "mechanism probe source differs from signed scenario selection"
        );
        Ok(())
    }

    /// Rejoin actual root ownership and its complete sealed purpose inside
    /// the caller's ORIGINAL preparation window. Selection captured before
    /// an attempt existed cannot mint an attempt deadline or admission.
    pub fn require_original_root_for_preparation(
        &self,
        state: &AppState,
        key: &ScopedProducerAuthorityKey,
        purpose: &QualificationExecutionPurposeView<'_>,
        scenario: &str,
        selected: &CurrentBundleProducerRecipe,
        coordinate: &ScopedProducerAttemptCoordinate,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        state.engine.with_checked_bundle_generation(|generation| {
            self.require_original_root_in_generation(
                state, generation, key, purpose, scenario, selected, coordinate, deadline,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn require_original_root_in_generation(
        &self,
        state: &AppState,
        generation: &CheckedEngineGeneration<'_>,
        key: &ScopedProducerAuthorityKey,
        purpose: &QualificationExecutionPurposeView<'_>,
        scenario: &str,
        selected: &CurrentBundleProducerRecipe,
        coordinate: &ScopedProducerAttemptCoordinate,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        ensure!(
            !deadline.has_elapsed(),
            "mechanism preparation deadline elapsed"
        );
        self.require_root_key(key)?;
        let admitted_stdin =
            super::admitted_root_producer_stdin(state, &key.root_thread_id, purpose)?;
        state.state_store.assert_launch_owner(
            &key.root_thread_id,
            &serde_json::to_string(&key.launch_owner)?,
        )?;
        let capsule = state
            .state_store
            .admitted_launch_capsule(&key.root_thread_id)?
            .context("mechanism preparation lost its original root capsule")?;
        ensure!(
            capsule.content_hash()? == self.root_capsule_hash,
            "mechanism preparation changed its original root capsule"
        );
        let current = super::resolve_current_bundle_producer_recipe_for_purpose_in_generation(
            state, generation, purpose, scenario,
        )?;
        ensure!(
            current == *selected
                && current.bundle_generation_identity
                    == generation.request_engine_generation_identity()
                && self.recipe_source.bundle_generation_identity
                    == generation.request_engine_generation_identity(),
            "mechanism recipe changed before preparation"
        );
        self.require_recipe(scenario, &current.source_identity()?, &current.recipe)?;
        ensure!(
            ScopedProducerAttemptCoordinate::derive(
                key,
                scenario,
                &current.source_identity()?,
                &admitted_stdin,
            )? == *coordinate,
            "mechanism preparation differs from original scoped attempt"
        );
        ensure!(
            !deadline.has_elapsed(),
            "mechanism preparation deadline elapsed"
        );
        Ok(())
    }

    /// Finalize and compile only this original root's retained signed Tool
    /// dependency. Its source and external CAS roots remain staged with the
    /// plan; no native guest input or helper-launch authority is produced.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_original_probe_member_plan(
        &self,
        state: &AppState,
        key: &ScopedProducerAuthorityKey,
        purpose: &QualificationExecutionPurposeView<'_>,
        scenario: &str,
        selected: &CurrentBundleProducerRecipe,
        coordinate: &ScopedProducerAttemptCoordinate,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<PreparedMechanismProbeProgram> {
        state.engine.with_checked_bundle_generation(|generation| {
            self.require_original_root_in_generation(
                state, generation, key, purpose, scenario, selected, coordinate, deadline,
            )?;
            self.require_current_signed_tool_source(state, generation, purpose)?;
            let engine = state.engine.as_ref();
            let kind = self.verified.resolved.kind.clone();
            let roots = engine.resolution_roots(None);
            let mut resolution = self.resolution.clone();
            super::require_reproducible_current_verifier_lane(&resolution.composed.derived, false)?;
            let schema = engine
                .kinds
                .get(&kind)
                .context("mechanism Tool has no signed kind schema")?;
            let execution = schema
                .execution()
                .context("mechanism Tool has no signed execution contract")?;
            let contract = schema.external_content_contract();
            let declarer = ryeos_engine::external_content::declaring_authority(&resolution)?;
            let shape = ryeos_engine::external_content::authored_external_content_shape(
                &resolution.composed.composed,
                contract,
                declarer,
            )?
            .context("mechanism Tool has no signed external content")?;
            ensure!(
                shape.product_slots.is_empty(),
                "mechanism Tool has an unselected product slot"
            );
            let declarations =
                ryeos_engine::external_content::effective_external_content_declarations(
                    &resolution,
                    contract,
                    declarer,
                )?
                .context("mechanism Tool has no fixed-pin declaration")?;
            ensure!(
                !declarations.is_empty()
                    && declarations.iter().all(|declaration| {
                        declaration.mode
                            == ryeos_engine::external_content::ExternalContentMode::Pinned
                            && declaration.locator.is_none()
                            && declaration.bundle_binary.is_none()
                            && declaration.digest.is_some()
                    }),
                "mechanism Tool requires only locator-free fixed pins"
            );

            // Use the same source, hook, external-realization and semantic
            // finalization owners as an ordinary current Tool admission.
            let source_contract = execution.source_closure.as_ref();
            let source_policy = if source_contract.is_some() {
                let executor_id = resolution
                    .composed
                    .composed
                    .get("executor_id")
                    .and_then(Value::as_str)
                    .context("mechanism Tool has no executor chain")?;
                ryeos_engine::launch::plan_builder::resolve_executor_source_policy(
                    executor_id,
                    &resolution.root.source_path,
                    &kind,
                    &engine.kinds,
                    &engine.parser_dispatcher,
                    &roots,
                    &engine.trust_store,
                    &engine.node_trust_store,
                    None,
                )?
            } else {
                None
            };
            self.require_original_root_in_generation(
                state, generation, key, purpose, scenario, selected, coordinate, deadline,
            )?;
            let mut publication = None;
            let source = crate::source_closure_admission::admit_source_closure_in_publication(
                state,
                engine,
                &kind,
                &mut resolution,
                &roots,
                None,
                source_policy.as_ref(),
                &mut publication,
                None,
            )?;
            self.require_original_root_in_generation(
                state, generation, key, purpose, scenario, selected, coordinate, deadline,
            )?;
            let config_snapshots =
                crate::effective_program_preparation::prepare_preselection_effective_program(
                    engine,
                    &self.verified.resolved,
                    &mut resolution,
                    &roots,
                    &engine.trust_store,
                    None,
                )?;
            super::require_reproducible_current_verifier_lane(&resolution.composed.derived, false)?;
            self.require_original_root_in_generation(
                state, generation, key, purpose, scenario, selected, coordinate, deadline,
            )?;
            let external =
                crate::external_content_admission::admit_external_realizations_in_publication(
                    state,
                    engine,
                    &kind,
                    &mut resolution,
                    &roots,
                    &SubjectResolutionAuthority::Projectless,
                    None,
                    &mut publication,
                )?
                .context("mechanism Tool produced no current external realization")?;
            self.require_original_root_in_generation(
                state, generation, key, purpose, scenario, selected, coordinate, deadline,
            )?;
            let semantic_projection =
                ryeos_engine::effective_program::take_recovered_effective_program_derived(
                    &mut resolution,
                );
            let validation = engine.effective_validators.validate(&kind, &resolution)?;
            let candidate = ryeos_engine::effective_program::relock_recovered_effective_program(
                resolution,
                validation,
                semantic_projection,
            )?;
            let config_roots = engine.launch_config_roots(&roots);
            let config_proofs = config_snapshots
                .as_ref()
                .map(|snapshots| std::slice::from_ref(&snapshots.dependency_proof))
                .unwrap_or_default();
            let finalization = ryeos_engine::effective_program::prove_finalization_authority(
                &candidate,
                contract,
                config_proofs,
                &config_roots,
                None,
                Some(external.finalization_evidence()),
                source.as_ref().map(|held| held.finalization_evidence()),
            )?;
            let finalized = ryeos_engine::effective_program::finalize_effective_program(
                candidate,
                finalization,
            )?;
            self.require_original_root_in_generation(
                state, generation, key, purpose, scenario, selected, coordinate, deadline,
            )?;
            self.require_finalized_captured_tool(
                &finalized,
                execution
                    .hooks
                    .as_ref()
                    .filter(|_| config_snapshots.is_some())
                    .map(|hooks| hooks.plan_derived.as_str()),
            )?;

            let canonical = CanonicalRef::parse(&self.resolution.root.resolved_ref)?;
            let site = state.threads.site_id().to_owned();
            let context = PlanContext {
                // Static capture above has no execution capability. Only this
                // exact signed Tool receives the scope needed by plan build.
                requested_by: EffectivePrincipal::Local(Principal {
                    fingerprint: purpose.owner_fingerprint().to_owned(),
                    scopes: vec![ryeos_runtime::authorizer::canonical_cap(
                        &canonical.kind,
                        &canonical.bare_id,
                        "execute",
                    )],
                }),
                project_context: ProjectContext::None,
                subject_resolution_authority: SubjectResolutionAuthority::Projectless,
                current_site_id: site.clone(),
                origin_site_id: site,
                execution_hints: ExecutionHints::default(),
                scheduled_fire: None,
                validate_only: false,
            };
            let executor_ref = self
                .verified
                .resolved
                .metadata
                .executor_id
                .as_ref()
                .filter(|value| !value.is_empty() && value.trim() == value.as_str())
                .context("mechanism Tool has no signed root executor identity")?
                .clone();
            let request = crate::thread_lifecycle::ResolvedExecutionRequest {
                kind: self.verified.resolved.kind.clone(),
                item_ref: canonical.to_string(),
                executor_ref,
                launch_mode: "wait".to_owned(),
                current_site_id: context.current_site_id.clone(),
                origin_site_id: context.origin_site_id.clone(),
                target_site_id: None,
                requested_by: Some(purpose.owner_fingerprint().to_owned()),
                usage_subject: None,
                usage_subject_asserted_by: None,
                parameters: serde_json::to_value(&self.parameters)?,
                ref_bindings: BTreeMap::new(),
                product_selections: Vec::new(),
                resolved_item: self.verified.resolved.clone(),
                root_raw_content_digest: self.verified.resolved.raw_content_digest.clone(),
                plan_context: context,
                root_admission: None,
            };
            let protocol_ref = match execution
                .terminator
                .as_ref()
                .context("mechanism Tool has no signed subprocess terminator")?
            {
                ryeos_engine::kind_registry::TerminatorDecl::Subprocess { protocol } => protocol
                    .resolve(&finalized.resolution().composed.composed)
                    .map_err(anyhow::Error::msg)?,
                ryeos_engine::kind_registry::TerminatorDecl::InProcess { .. } => {
                    anyhow::bail!("mechanism Tool has no subprocess protocol")
                }
            };
            let protocol = state.engine.protocols.require(&protocol_ref)?.clone();
            let (prepared, closure, artifact) = {
                let authority = publication
                    .as_ref()
                    .context("mechanism realization has no retained CAS publication")?
                    .authority()
                    .try_clone()?;
                let guard = authority.acquire_shared_guard()?;
                let plan = crate::thread_lifecycle::prepare_captured_external_member_plan(
                    state,
                    state.engine.as_ref(),
                    &request,
                    &self.verified,
                    &finalized,
                    None,
                    ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::CapturedExecution,
                    &authority,
                    &guard,
                    state.isolation.verified_command_file_bytes(),
                )?;
                ensure!(
                    plan.execution_plan()
                        .executor_chain
                        .first()
                        .map(String::as_str)
                        == Some(request.item_ref.as_str())
                        && plan
                            .execution_plan()
                            .executor_chain
                            .get(1)
                            .map(String::as_str)
                            == Some(request.executor_ref.as_str()),
                    "mechanism compiled plan changed its signed root executor"
                );
                let (closure, artifact) = plan.mechanism_direct_closure(
                    &request,
                    &protocol,
                    &state.engine.node_trust_store,
                    &authority,
                    &guard,
                )?;
                authority.ensure_guard(&guard)?;
                (plan, closure, artifact)
            };
            self.require_original_root_in_generation(
                state, generation, key, purpose, scenario, selected, coordinate, deadline,
            )?;
            self.require_current_signed_tool_source(state, generation, purpose)?;
            ensure!(
                !deadline.has_elapsed(),
                "mechanism preparation deadline elapsed after source recapture"
            );
            let publication =
                publication.context("mechanism realization lost its staged CAS publication")?;
            Ok(PreparedMechanismProbeProgram {
                root_key: key.clone(),
                root_capsule_hash: self.root_capsule_hash.clone(),
                scenario_id: self.scenario_id.clone(),
                recipe_source: self.recipe_source.clone(),
                coordinate: coordinate.clone(),
                deadline,
                finalized,
                request,
                protocol,
                closure,
                artifact,
                plan: prepared,
                source,
                external,
                publication,
                config_snapshots,
            })
        })
    }

    /// Copy only the actual signed fixture snapshot CAS closure. The original
    /// program, source, product, config and publication lifelines remain owned
    /// together with the measured descriptor; no project authority is minted.
    pub(crate) fn prepare_original_fixture_transfer(
        &self,
        state: &AppState,
        purpose: &QualificationExecutionPurposeView<'_>,
        selected: &CurrentBundleProducerRecipe,
        program: PreparedMechanismProbeProgram,
    ) -> Result<PreparedMechanismFixtureTransfer> {
        // Validate the exact signed shape and original owner before allocating
        // a private CAS transfer or reading a selected object.
        let selected_snapshot_hash = self.selected_fixture_snapshot_hash()?.to_owned();
        ensure!(
            program.root_key == self.root_key
                && program.root_capsule_hash == self.root_capsule_hash
                && program.scenario_id == self.scenario_id
                && program.recipe_source == self.recipe_source
                && !program.deadline.has_elapsed(),
            "mechanism fixture differs from its original prepared root"
        );
        state.engine.with_checked_bundle_generation(|generation| {
            self.require_original_root_in_generation(
                state,
                generation,
                &program.root_key,
                purpose,
                &program.scenario_id,
                selected,
                &program.coordinate,
                program.deadline,
            )?;
            self.require_current_signed_tool_source(state, generation, purpose)?;
            let current = program.finalized.resolution();
            let hooks = state
                .engine
                .kinds
                .get(&self.verified.resolved.kind)
                .and_then(|schema| schema.execution.as_ref())
                .and_then(|execution| execution.hooks.as_ref());
            self.require_finalized_captured_tool(
                &program.finalized,
                hooks
                    .filter(|_| program.config_snapshots.is_some())
                    .map(|hooks| hooks.plan_derived.as_str()),
            )?;
            ensure!(
                program.request.item_ref == current.root.resolved_ref
                    && program
                        .plan
                        .execution_plan()
                        .executor_chain
                        .get(1)
                        .map(String::as_str)
                        == Some(program.request.executor_ref.as_str())
                    && program.request.root_raw_content_digest
                        == self.verified.resolved.raw_content_digest,
                "mechanism byte custody changed its captured request identity"
            );
            let execution = state
                .engine
                .kinds
                .get(&self.verified.resolved.kind)
                .and_then(|schema| schema.execution.as_ref())
                .context("mechanism Tool lost its signed execution declaration")?;
            let protocol_ref = match execution
                .terminator
                .as_ref()
                .context("mechanism Tool lost its signed terminator")?
            {
                ryeos_engine::kind_registry::TerminatorDecl::Subprocess { protocol } => protocol
                    .resolve(&current.composed.composed)
                    .map_err(anyhow::Error::msg)?,
                ryeos_engine::kind_registry::TerminatorDecl::InProcess { .. } => {
                    anyhow::bail!("mechanism Tool changed its subprocess terminator")
                }
            };
            let current_protocol = state.engine.protocols.require(&protocol_ref)?;
            ensure!(
                current_protocol.canonical_ref == program.protocol.canonical_ref
                    && current_protocol.raw_content_digest == program.protocol.raw_content_digest
                    && current_protocol.signer_fingerprint == program.protocol.signer_fingerprint
                    && serde_json::to_value(&current_protocol.descriptor)?
                        == serde_json::to_value(&program.protocol.descriptor)?,
                "mechanism byte custody changed its signed protocol"
            );
            let source_projection = current
                .composed
                .derived
                .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
                .map(ryeos_state::objects::EffectiveSourceClosureProjection::from_value)
                .transpose()?;
            ensure!(
                match (&program.source, source_projection.as_ref()) {
                    (None, None) => true,
                    (Some(source), Some(projection)) => {
                        source.finalization_evidence().0.projection() == projection
                    }
                    _ => false,
                },
                "mechanism fixture changed its admitted B source"
            );
            let realized =
                ryeos_engine::external_realization::RealizedExternalContentSet::from_value(
                    current
                        .composed
                        .derived
                        .get(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
                        .context("mechanism fixture lost its admitted realization set")?,
                )?;
            ensure!(
                program.external.finalization_evidence().0.realized() == &realized
                    && program.plan.execution_plan().root_ref == current.root.resolved_ref
                    && program.plan.realization_command().is_some(),
                "mechanism fixture changed its finalized member or product authority"
            );
            let authority = program.publication.authority().try_clone()?;
            let state_authority = state.state_store.pinned_state_authority()?;
            ensure!(
                authority.cas_directory().identity()?
                    == state_authority.cas_directory().identity()?,
                "mechanism fixture CAS differs from its prepared program authority"
            );
            self.require_original_root_in_generation(
                state,
                generation,
                &program.root_key,
                purpose,
                &program.scenario_id,
                selected,
                &program.coordinate,
                program.deadline,
            )?;
            let (transfer, measurement, descriptor) = {
                let guard = authority.acquire_shared_guard()?;
                authority.ensure_guard(&guard)?;
                let cas = authority.cas_store()?;
                let limits = state
                    .node_policy
                    .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
                    .closure_limits()?;
                let snapshot_value =
                    ryeos_state::object_closure::load_exact_cas_object_with_cas(
                        &cas,
                        &selected_snapshot_hash,
                        limits.max_object_bytes,
                    )
                    .context("signed mechanism fixture ProjectSnapshot is absent or invalid")?;
                ensure!(
                    lillux::sha256_hex(lillux::canonical_json(&snapshot_value)?.as_bytes())
                        == selected_snapshot_hash,
                    "signed mechanism fixture ProjectSnapshot changed its CAS address"
                );
                ryeos_state::objects::ProjectSnapshot::from_value(&snapshot_value)?;
                let closure =
                    ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
                        &cas,
                        [selected_snapshot_hash.clone()],
                        limits,
                    )?;
                ensure!(
                    closure.is_complete() && closure.large_object_hashes.is_empty(),
                    "signed mechanism fixture CAS closure is incomplete or non-CAS"
                );
                ensure!(
                    !program.deadline.has_elapsed(),
                    "mechanism fixture deadline elapsed after closure preflight"
                );
                let transfer = ryeos_project_capture::prepare_project_snapshot_transfer(
                    &authority,
                    &guard,
                    &selected_snapshot_hash,
                )?;
                let measurement = transfer.measurement();
                ensure!(
                    measurement.snapshot_hash == selected_snapshot_hash
                        && measurement.object_count == u64::try_from(closure.object_hashes.len())?
                        && measurement.blob_count == u64::try_from(closure.blob_hashes.len())?
                        && measurement.total_bytes
                            <= limits
                                .max_total_object_bytes
                                .saturating_add(limits.max_total_blob_bytes),
                    "mechanism fixture transfer changed its signed bounded closure"
                );
                let descriptor = transfer.descriptor();
                descriptor.directory_identity()?;
                authority.ensure_guard(&guard)?;
                (transfer, measurement, descriptor)
            };
            self.require_original_root_in_generation(
                state,
                generation,
                &program.root_key,
                purpose,
                &program.scenario_id,
                selected,
                &program.coordinate,
                program.deadline,
            )?;
            self.require_current_signed_tool_source(state, generation, purpose)?;
            ensure!(
                !program.deadline.has_elapsed(),
                "mechanism fixture deadline elapsed after final source recapture"
            );
            Ok(PreparedMechanismFixtureTransfer {
                selected_snapshot_hash,
                measurement,
                descriptor,
                transfer,
                program,
            })
        })
    }

    /// Lend actual B proof and measured fixture bytes to the trusted Executor
    /// materializer, then compile inert program data from its returned exact
    /// descriptor authority. This does not grant project, guest or helper
    /// execution. The trusted callback can perform unrelated side effects;
    /// these guards authenticate only the inputs and result window.
    pub fn with_original_fixture_program_data<T: 'static>(
        &self,
        state: &AppState,
        purpose: &QualificationExecutionPurposeView<'_>,
        selected: &CurrentBundleProducerRecipe,
        fixture: &PreparedMechanismFixtureTransfer,
        materialize: impl FnOnce(
            &ResolutionOutput,
            Option<&crate::source_closure_admission::AdmittedSourceClosure>,
            &crate::external_content_admission::AdmittedExternalRealizations,
            &ryeos_project_capture::ProjectSnapshotTransferMeasurement,
            &lillux::InheritedDescriptorAuthority,
            &BTreeMap<String, String>,
            &[String],
            ryeos_external_execution_contract::GuestWorkspaceView,
            lillux::time::MonotonicDeadline,
        ) -> Result<(
            ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
            Option<ryeos_state::source_verification::VerifiedAdmittedSourceRecords>,
            T,
        )>,
    ) -> Result<(
        ryeos_state::external_execution::admission::AdmittedExternalDirectProgram,
        ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
        T,
    )> {
        let program = &fixture.program;
        ensure!(
            program.root_key == self.root_key
                && program.root_capsule_hash == self.root_capsule_hash
                && program.scenario_id == self.scenario_id
                && program.recipe_source == self.recipe_source
                && !program.deadline.has_elapsed(),
            "mechanism byte custody differs from its original prepared root"
        );
        let selected_snapshot_hash = self.selected_fixture_snapshot_hash()?;
        ensure!(
            fixture.selected_snapshot_hash == selected_snapshot_hash
                && fixture.measurement.snapshot_hash == selected_snapshot_hash
                && fixture.transfer.measurement() == fixture.measurement
                && fixture.descriptor.directory_identity()?
                    == fixture.transfer.descriptor().directory_identity()?,
            "mechanism byte custody changed its original signed fixture transfer"
        );
        state.engine.with_checked_bundle_generation(|generation| {
            self.require_original_root_in_generation(
                state,
                generation,
                &program.root_key,
                purpose,
                &program.scenario_id,
                selected,
                &program.coordinate,
                program.deadline,
            )?;
            self.require_current_signed_tool_source(state, generation, purpose)?;
            let current = program.finalized.resolution();
            let hooks = state
                .engine
                .kinds
                .get(&self.verified.resolved.kind)
                .and_then(|schema| schema.execution.as_ref())
                .and_then(|execution| execution.hooks.as_ref());
            self.require_finalized_captured_tool(
                &program.finalized,
                hooks
                    .filter(|_| program.config_snapshots.is_some())
                    .map(|hooks| hooks.plan_derived.as_str()),
            )?;
            let source_projection = current
                .composed
                .derived
                .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
                .map(ryeos_state::objects::EffectiveSourceClosureProjection::from_value)
                .transpose()?;
            ensure!(
                match (&program.source, source_projection.as_ref()) {
                    (None, None) => true,
                    (Some(source), Some(projection)) => {
                        source.finalization_evidence().0.projection() == projection
                    }
                    _ => false,
                },
                "mechanism byte custody changed its admitted B source"
            );
            let realized =
                ryeos_engine::external_realization::RealizedExternalContentSet::from_value(
                    current
                        .composed
                        .derived
                        .get(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
                        .context("mechanism byte custody lost its admitted realization set")?,
                )?;
            ensure!(
                program.external.finalization_evidence().0.realized() == &realized
                    && program.plan.execution_plan().root_ref == current.root.resolved_ref
                    && program.plan.realization_command().is_some(),
                "mechanism byte custody changed its finalized member or product authority"
            );
            let authority = program.publication.authority();
            let state_authority = state.state_store.pinned_state_authority()?;
            ensure!(
                authority.cas_directory().identity()?
                    == state_authority.cas_directory().identity()?,
                "mechanism byte custody CAS differs from its retained program"
            );
            ensure!(
                !program.deadline.has_elapsed(),
                "mechanism byte custody deadline elapsed before materialization"
            );
            let (chain_root_id, capsule_hash, _) = state
                .state_store
                .admitted_launch_capsule_with_coordinates(&program.root_key.root_thread_id)?
                .context("mechanism program lost its original root coordinate")?;
            ensure!(
                chain_root_id == program.root_key.root_thread_id
                    && capsule_hash == program.root_capsule_hash,
                "mechanism program changed its enclosing verifier root"
            );
            ensure!(
                program.request.item_ref == current.root.resolved_ref
                    && program
                        .plan
                        .execution_plan()
                        .executor_chain
                        .get(1)
                        .map(String::as_str)
                        == Some(program.request.executor_ref.as_str())
                    && program.request.root_raw_content_digest
                        == self.verified.resolved.raw_content_digest,
                "mechanism byte custody changed its captured request identity"
            );
            let execution = state
                .engine
                .kinds
                .get(&self.verified.resolved.kind)
                .and_then(|schema| schema.execution.as_ref())
                .context("mechanism Tool lost its signed execution declaration")?;
            let protocol_ref = match execution
                .terminator
                .as_ref()
                .context("mechanism Tool lost its signed terminator")?
            {
                ryeos_engine::kind_registry::TerminatorDecl::Subprocess { protocol } => protocol
                    .resolve(&current.composed.composed)
                    .map_err(anyhow::Error::msg)?,
                ryeos_engine::kind_registry::TerminatorDecl::InProcess { .. } => {
                    anyhow::bail!("mechanism Tool changed its subprocess terminator")
                }
            };
            let current_protocol = state.engine.protocols.require(&protocol_ref)?;
            ensure!(
                current_protocol.canonical_ref == program.protocol.canonical_ref
                    && current_protocol.raw_content_digest == program.protocol.raw_content_digest
                    && current_protocol.signer_fingerprint == program.protocol.signer_fingerprint
                    && serde_json::to_value(&current_protocol.descriptor)?
                        == serde_json::to_value(&program.protocol.descriptor)?,
                "mechanism byte custody changed its signed protocol"
            );
            let expected_source = program
                .source
                .as_ref()
                .map(|source| {
                    let projection = source.finalization_evidence().0.projection();
                    let binding = lillux::canonical_json(&source.binding().to_value()?)?;
                    let manifest = lillux::canonical_json(&source.manifest().to_value()?)?;
                    ryeos_state::source_verification::VerifiedAdmittedSourceRecords::from_canonical_bytes(
                        &projection.binding_hash,
                        &projection.content_manifest_hash,
                        binding.as_bytes(),
                        manifest.as_bytes(),
                    )
                })
                .transpose()?;
            let (environment, search, workspace) = program.plan.projectless_mechanism_environment(
                &program.protocol,
                &program.root_key.root_thread_id,
                expected_source.as_ref(),
            )?;
            // No CAS guard spans Executor's source/product materializers or
            // owner cleanup. T is owned, so proof borrows cannot escape.
            let materialized = materialize(
                current,
                program.source.as_ref(),
                &program.external,
                &fixture.measurement,
                &fixture.descriptor,
                &environment,
                &search,
                workspace,
                program.deadline,
            )?;
            ensure!(
                materialized.0.projection().base_snapshot.snapshot_hash
                    == fixture.measurement.snapshot_hash
                    && materialized.0.projection().base_snapshot.closure_digest
                        == fixture.measurement.closure_digest
                    && materialized.0.projection().base_snapshot.object_count
                        == fixture.measurement.object_count
                    && materialized.0.projection().base_snapshot.blob_count
                        == fixture.measurement.blob_count
                    && materialized.0.projection().base_snapshot.total_bytes
                        == fixture.measurement.total_bytes
                    && materialized.0.projection().base_snapshot.descriptor
                        == fixture.descriptor.inherited_descriptor().map_err(anyhow::Error::msg)?
                    && materialized.0.projection().workspace_outputs.is_none()
                    && materialized.0.projection().environment == environment
                    && materialized.0.projection().executable_search == search
                    && materialized.0.projection().workspace_view == workspace,
                "mechanism guest inputs changed original fixture or pure plan facts"
            );
            let delivered_products = materialized
                .0
                .projection()
                .inputs
                .iter()
                .filter(|input| {
                    input.role == ryeos_external_execution_contract::GuestMountRole::Product
                })
                .collect::<Vec<_>>();
            ensure!(
                delivered_products.len() == realized.iter().len()
                    && delivered_products.iter().zip(realized.iter()).all(|(input, entry)| {
                        let destination = entry.mount_root.destination(
                            Some(std::path::Path::new("/workspace")),
                            &entry.mount,
                        );
                        let ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                            manifest_hash,
                            ..
                        } = &input.content_authority else {
                            return false;
                        };
                        destination
                            .as_ref()
                            .ok()
                            .and_then(|path| path.to_str())
                            == Some(input.destination.as_str())
                            && input.authority_id == entry.id
                            && manifest_hash == &entry.manifest_hash
                            && input.bytes == entry.total_bytes
                            && input.access
                                == ryeos_external_execution_contract::GuestMountAccess::ReadOnly
                            && match entry.kind {
                                ryeos_state::objects::ExternalContentKind::Tree => {
                                    input.kind
                                        == ryeos_external_execution_contract::GuestMountKind::Directory
                                        && input.normalized_mode.is_none()
                                }
                                ryeos_state::objects::ExternalContentKind::File => {
                                    input.kind
                                        == ryeos_external_execution_contract::GuestMountKind::RegularFile
                                        && input.normalized_mode.is_some()
                                }
                            }
                    })
                    && materialized.0.projection().inputs.len()
                        == delivered_products.len() + usize::from(expected_source.is_some()),
                "mechanism guest products differ from the complete admitted realization inventory"
            );
            ensure!(
                match (expected_source.as_ref(), materialized.1.as_ref()) {
                    (None, None) => true,
                    (Some(expected), Some(actual)) => {
                        expected.binding_hash() == actual.binding_hash()
                            && expected.content_manifest_hash() == actual.content_manifest_hash()
                            && expected.owner_key() == actual.owner_key()
                            && expected.binding() == actual.binding()
                            && expected.manifest() == actual.manifest()
                            && expected.runtime_destination() == actual.runtime_destination()
                            && expected.runtime_entry_path() == actual.runtime_entry_path()
                    }
                    _ => false,
                },
                "mechanism delivered B source differs from admitted source records"
            );
            let direct = program.plan.compile_projectless_mechanism_parts(
                &program.closure,
                &program.artifact,
                &program.protocol,
                &program.root_key.root_thread_id,
                materialized.0.projection(),
                materialized.1.as_ref(),
            )?;
            ensure!(
                fixture.selected_snapshot_hash == self.selected_fixture_snapshot_hash()?
                    && fixture.transfer.measurement() == fixture.measurement
                    && fixture.descriptor.directory_identity()?
                        == fixture.transfer.descriptor().directory_identity()?
                    && authority.cas_directory().identity()?
                        == state_authority.cas_directory().identity()?,
                "mechanism byte custody changed its retained fixture or CAS authority"
            );
            self.require_original_root_in_generation(
                state,
                generation,
                &program.root_key,
                purpose,
                &program.scenario_id,
                selected,
                &program.coordinate,
                program.deadline,
            )?;
            self.require_current_signed_tool_source(state, generation, purpose)?;
            ensure!(
                !program.deadline.has_elapsed(),
                "mechanism byte custody deadline elapsed after final source recapture"
            );
            Ok((direct, materialized.0, materialized.2))
        })
    }

    fn require_current_signed_tool_source(
        &self,
        state: &AppState,
        generation: &CheckedEngineGeneration<'_>,
        purpose: &QualificationExecutionPurposeView<'_>,
    ) -> Result<()> {
        let canonical = CanonicalRef::parse(&self.resolution.root.resolved_ref)?;
        let site = state.threads.site_id().to_owned();
        let context = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: purpose.owner_fingerprint().to_owned(),
                scopes: vec![],
            }),
            project_context: ProjectContext::None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            current_site_id: site.clone(),
            origin_site_id: site,
            execution_hints: ExecutionHints::default(),
            scheduled_fire: None,
            validate_only: true,
        };
        let verified = generation.verify(&context, generation.resolve(&context, &canonical)?)?;
        let resolution = super::resolve_consumer_definition_in_generation(
            generation,
            &self.resolution.root.resolved_ref,
            "tool",
        )?;
        ensure!(
            verified.resolved.canonical_ref == self.verified.resolved.canonical_ref
                && verified.resolved.raw_content_digest
                    == self.verified.resolved.raw_content_digest
                && verified.resolved.content_hash == self.verified.resolved.content_hash
                && verified.resolved.source_space == self.verified.resolved.source_space
                && verified.resolved.source_root == self.verified.resolved.source_root
                && verified.signer == self.verified.signer
                && verified.trust_class == self.verified.trust_class
                && serde_json::to_value(&resolution)? == serde_json::to_value(&self.resolution)?,
            "mechanism probe Tool changed after original source capture"
        );
        let sources = generation.capture_verified_signed_bundle_sources(&resolution)?;
        ensure!(
            sources.len() == self.signed_sources.len()
                && sources.iter().zip(&self.signed_sources).all(|(now, held)| {
                    now.resolved_ref == held.resolved_ref
                        && now.source_root == held.source_root
                        && now.signer_fingerprint == held.signer_fingerprint
                        && now.source_content_digest == held.source_content_digest
                        && now.raw_content_digest == held.raw_content_digest
                        && now.signature_envelope == held.signature_envelope
                        && now.signed_bytes == held.signed_bytes
                }),
            "mechanism probe signed Tool envelope changed after original capture"
        );
        let manifests = generation.capture_verified_source_bundle_manifests(&sources)?;
        ensure!(
            manifests.len() == self.signed_bundle_manifests.len()
                && manifests
                    .iter()
                    .zip(&self.signed_bundle_manifests)
                    .all(|(now, held)| {
                        now.identity == held.identity && now.signed_bytes == held.signed_bytes
                    }),
            "mechanism probe signed Bundle manifest changed after original capture"
        );
        Ok(())
    }

    fn require_finalized_captured_tool(
        &self,
        finalized: &ryeos_engine::effective_program::FinalizedEffectiveProgram,
        owned_hook_projection: Option<&str>,
    ) -> Result<()> {
        let held = &self.resolution;
        let current = finalized.resolution();
        let authored = |resolution: &ResolutionOutput| {
            serde_json::json!({
                "root": &resolution.root,
                "ancestors": &resolution.ancestors,
                "references_edges": &resolution.references_edges,
                "referenced_items": &resolution.referenced_items,
                "step_outputs": &resolution.step_outputs,
                "effective_trust_class": resolution.effective_trust_class,
                "composed": &resolution.composed.composed,
                "policy_facts": &resolution.composed.policy_facts,
            })
        };
        ensure!(
            authored(held) == authored(current)
                && held
                    .composed
                    .derived
                    .iter()
                    .all(|(name, value)| { current.composed.derived.get(name) == Some(value) })
                && current.composed.derived.keys().all(|name| {
                    held.composed.derived.contains_key(name)
                        || owned_hook_projection == Some(name.as_str())
                        || matches!(
                            name.as_str(),
                            ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY
                                | ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY
                                | ryeos_effect_contract::EFFECT_AUTHORIZATIONS_DERIVED_KEY
                        )
                }),
            "finalized mechanism Tool differs from captured signed source"
        );
        Ok(())
    }

    pub fn root_capsule_hash(&self) -> &str {
        &self.root_capsule_hash
    }
    pub fn verified_subject(&self) -> &VerifiedItem {
        &self.verified
    }
    pub fn resolution(&self) -> &ResolutionOutput {
        &self.resolution
    }
    pub fn parameters(&self) -> &BTreeMap<String, Value> {
        &self.parameters
    }
    pub fn signed_sources(&self) -> &[CapturedSignedBundleItemSource] {
        &self.signed_sources
    }
    pub fn signed_bundle_manifests(
        &self,
    ) -> &[ryeos_engine::plan_builder::CapturedSignedBundleManifest] {
        &self.signed_bundle_manifests
    }
}

/// Capture the signed Tool dependency before registration and verifier spawn.
/// This uses the original born root, not a synthetic probe capsule or thread.
/// Both definition resolution and signed-envelope capture share the exact
/// generation admitted by the producer recipe. Missing inputs fail closed.
pub fn capture_root_mechanism_probe_source(
    state: &AppState,
    key: &ScopedProducerAuthorityKey,
    purpose: &QualificationExecutionPurposeView<'_>,
    scenario_id: &str,
    selected: &CurrentBundleProducerRecipe,
) -> Result<CapturedMechanismProbeSource> {
    ensure!(
        purpose
            .policy_source()
            .policy
            .consumer_execution_context
            .is_none()
            && matches!(
                selected.recipe.executable_source,
                ProducerExecutableSource::AdmittedMechanismHelpers { .. }
            ),
        "mechanism probe preparation requires a non-consumer helper purpose"
    );
    // Authenticate the FULL sealed enclosing purpose, rather than only its
    // subject fields. The returned private parameters are not native authority.
    super::admitted_root_producer_stdin(state, &key.root_thread_id, purpose)?;
    state.state_store.assert_launch_owner(
        &key.root_thread_id,
        &serde_json::to_string(&key.launch_owner)?,
    )?;
    let capsule = state
        .state_store
        .admitted_launch_capsule(&key.root_thread_id)?
        .context("mechanism probe source has no original admitted root capsule")?;
    let root_capsule_hash = capsule.content_hash()?;
    state.engine.with_checked_bundle_generation(|generation| {
        let current = super::resolve_current_bundle_producer_recipe_for_purpose_in_generation(
            state,
            generation,
            purpose,
            scenario_id,
        )?;
        ensure!(
            current == *selected,
            "mechanism probe recipe changed before source capture"
        );
        let source = current.source_identity()?;
        ensure!(
            source.bundle_generation_identity == generation.request_engine_generation_identity(),
            "mechanism probe source changed its admitted Bundle generation"
        );
        let ProducerStdinSource::TrustedMechanismBootstrap {
            probe_ref,
            probe_parameters,
        } = &current.recipe.stdin_source
        else {
            anyhow::bail!("mechanism probe source has no signed Tool selection");
        };
        let canonical = CanonicalRef::parse(probe_ref)?;
        let site = state.threads.site_id().to_owned();
        // Resolve and verify signed source only. Static verification needs no
        // execution capability; a signed selection must not invent one for
        // later compilation or dispatch.
        let context = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: purpose.owner_fingerprint().to_owned(),
                scopes: vec![],
            }),
            project_context: ProjectContext::None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            current_site_id: site.clone(),
            origin_site_id: site,
            execution_hints: ExecutionHints::default(),
            scheduled_fire: None,
            validate_only: true,
        };
        let verified = generation.verify(&context, generation.resolve(&context, &canonical)?)?;
        let resolution =
            super::resolve_consumer_definition_in_generation(generation, probe_ref, "tool")?;
        ensure!(
            verified.resolved.canonical_ref.to_string() == resolution.root.resolved_ref
                && verified.resolved.raw_content_digest == resolution.root.raw_content_digest
                && verified.resolved.content_hash == resolution.root.source_content_digest
                && verified.resolved.source_space == resolution.root.source_space
                && verified.resolved.source_root == resolution.root.source_root
                && verified.trust_class == ryeos_engine::contracts::TrustClass::Trusted
                && verified.signer.as_ref().map(|signer| &signer.0)
                    == resolution.root.signer_fingerprint.as_ref(),
            "mechanism probe verified root differs from captured definition"
        );
        let signed_sources = generation.capture_verified_signed_bundle_sources(&resolution)?;
        let signed_bundle_manifests =
            generation.capture_verified_source_bundle_manifests(&signed_sources)?;
        Ok(CapturedMechanismProbeSource {
            root_key: key.clone(),
            root_capsule_hash,
            scenario_id: scenario_id.to_owned(),
            recipe_source: source,
            verified,
            resolution,
            parameters: probe_parameters.clone(),
            signed_sources,
            signed_bundle_manifests,
        })
    })
}
