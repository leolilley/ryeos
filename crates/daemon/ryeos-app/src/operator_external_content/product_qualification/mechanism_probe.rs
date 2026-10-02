//! Controller-retained signed probe source for one original qualification root.
//! This captures definition provenance, not a native program or launch permit.

use anyhow::{Context as _, Result, ensure};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
    SubjectResolutionAuthority, VerifiedItem,
};
use ryeos_engine::engine::{CapturedSignedBundleItemSource, CheckedEngineGeneration};
use ryeos_engine::resolution::ResolutionOutput;
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

impl CapturedMechanismProbeSource {
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

    /// Compile only this original root's retained signed Tool dependency.
    /// The returned plan still lacks materialized command authority, native
    /// guest inputs, and any permission to launch a helper or issue a claim.
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
        finalized: &ryeos_engine::effective_program::FinalizedEffectiveProgram,
    ) -> Result<crate::thread_lifecycle::PreparedItemPlan> {
        state.engine.with_checked_bundle_generation(|generation| {
            self.require_original_root_in_generation(
                state, generation, key, purpose, scenario, selected, coordinate, deadline,
            )?;
            self.require_current_signed_tool_source(state, generation, purpose)?;
            self.require_finalized_captured_tool(finalized)?;

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
            let request = crate::thread_lifecycle::ResolvedExecutionRequest {
                kind: self.verified.resolved.kind.clone(),
                item_ref: canonical.to_string(),
                executor_ref: String::new(),
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
            let prepared = {
                let authority = state.state_store.pinned_state_authority()?;
                let guard = authority.acquire_shared_guard()?;
                let plan = crate::thread_lifecycle::prepare_captured_external_member_plan(
                    state,
                    state.engine.as_ref(),
                    &request,
                    &self.verified,
                    finalized,
                    None,
                    ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::CapturedExecution,
                    &authority,
                    &guard,
                    state.isolation.verified_command_file_bytes(),
                )?;
                authority.ensure_guard(&guard)?;
                plan
            };
            self.require_original_root_in_generation(
                state, generation, key, purpose, scenario, selected, coordinate, deadline,
            )?;
            self.require_current_signed_tool_source(state, generation, purpose)?;
            ensure!(
                !deadline.has_elapsed(),
                "mechanism preparation deadline elapsed after source recapture"
            );
            Ok(prepared)
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
