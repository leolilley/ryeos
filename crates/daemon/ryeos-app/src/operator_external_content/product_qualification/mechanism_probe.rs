//! Controller-retained signed probe source for one original qualification root.
//! This captures definition provenance, not a native program or launch permit.

use anyhow::{Context as _, Result, ensure};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
    SubjectResolutionAuthority, VerifiedItem,
};
use ryeos_engine::engine::CapturedSignedBundleItemSource;
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
        let current =
            super::resolve_current_bundle_producer_recipe_for_purpose(state, purpose, scenario)?;
        ensure!(
            current == *selected,
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
