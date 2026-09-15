//! Non-spawning reconstruction of a current direct verifier's launch artifact.
//!
//! Product qualification is narrower than ordinary execution: the current
//! eligibility check requires exact executable provenance and therefore
//! refuses node-policy commands. This module does not publish an object, mint
//! a thread, or weaken ordinary direct execution's existing NodePolicy lane.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_engine::contracts::{EffectivePrincipal, ItemSpace, ProjectContext};
use ryeos_engine::resolution::TrustClass;
use ryeos_state::objects::{AdmittedLaunchArtifactIdentity, DirectExecutableIdentity};

use crate::handler_context::HandlerContext;
use crate::state::AppState;
use crate::thread_lifecycle::ResolvedExecutionRequest;

/// Reconstruct the exact current direct-launch artifact for an already
/// authenticated, projectless Bundle verifier and its already-proved D2.
///
/// The caller remains responsible for comparing the returned structured
/// identity with signed retained qualification testimony. `finalized_program`
/// must carry the same current source/selection/realization D2 whose authority
/// was checked against that testimony; this helper does not resolve products
/// or current qualification heads again. Root admission remains the verified
/// pre-augmentation D0/request authority.
pub(super) fn reconstruct_current_direct_artifact_identity(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    context: &HandlerContext,
    resolved: &ResolvedExecutionRequest,
    finalized_program: &ryeos_engine::effective_program::FinalizedEffectiveProgram,
    logical_project_root: Option<&std::path::Path>,
) -> anyhow::Result<AdmittedLaunchArtifactIdentity> {
    authority.ensure_guard(guard)?;
    // The verifier runs on this node, but its owner can be an authenticated
    // remote operator. Use the same admitted-owner boundary as qualification
    // publication; transport locality must not replace principal/site proof.
    crate::operator_authority::require_admitted_operator(state, context)?;

    state.engine.with_checked_bundle_generation(|_| {
        let admission = resolved
            .root_admission
            .as_ref()
            .context("current qualification verifier has no root admission")?;
        if !Arc::ptr_eq(admission.request_engine(), &state.engine) {
            bail!("current qualification verifier uses a different engine generation");
        }
        admission.ensure_matches_request(resolved)?;
        admission.ensure_matches_plan_context(&state.engine, &resolved.plan_context)?;

        let EffectivePrincipal::Local(principal) = &resolved.plan_context.requested_by else {
            bail!("independent product qualification rejects delegated verifier principals");
        };
        context.validate_execution_authority(
            &principal.fingerprint,
            &principal.scopes,
            &resolved.current_site_id,
            &resolved.origin_site_id,
        )?;
        if resolved.requested_by.as_deref() != Some(context.fingerprint.as_str()) {
            bail!("current qualification verifier owner differs from its handler authority");
        }
        if resolved.plan_context.project_context != ProjectContext::None
            || !matches!(
                admission.project_authority(),
                ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. }
            )
        {
            bail!("current direct qualification verifier must be projectless");
        }
        let verified_subject = admission.verified_subject();
        let finalized_resolution = finalized_program.resolution();
        if resolved.resolved_item.source_space != ItemSpace::Bundle
            || verified_subject.trust_class != ryeos_engine::contracts::TrustClass::Trusted
            || verified_subject.resolved.source_space != ItemSpace::Bundle
            || finalized_resolution.effective_trust_class != TrustClass::TrustedBundle
            || finalized_resolution.root.resolved_ref != resolved.item_ref
            || finalized_resolution.root.source_content_digest
                != resolved.resolved_item.content_hash
            || finalized_resolution.root.raw_content_digest
                != resolved.resolved_item.raw_content_digest
        {
            bail!("current qualification verifier is not the exact trusted Bundle executable");
        }
        if finalized_resolution
            .authored_definition_digest()
            .context("derive finalized current verifier authored identity")?
            != admission
                .resolution_output()
                .authored_definition_digest()
                .context("derive admitted current verifier authored identity")?
        {
            bail!("current qualification verifier artifact changed its admitted authored program");
        }

        let mut prepared = crate::thread_lifecycle::prepare_bundle_item_plan_for_qualification(
            &state.engine,
            resolved,
            state.isolation.as_ref(),
            finalized_program,
            logical_project_root,
        )?;
        prepared.bind_realization_command_guarded(
            authority,
            guard,
            &state.engine,
            finalized_resolution,
            state.isolation.as_ref(),
        )?;
        prepared.bind_logical_project_root(logical_project_root)?;
        let protocol =
            crate::thread_lifecycle::resolve_direct_terminator_protocol(&state.engine, resolved)?;
        let artifact = prepared.admitted_artifact_identity(resolved, protocol)?;
        let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executable_identity,
            ..
        } = &artifact
        else {
            bail!("current Tool verifier produced a non-direct artifact identity");
        };
        if matches!(executable_identity, DirectExecutableIdentity::NodePolicy) {
            bail!(
                "independent product qualification requires exact verifier executable provenance"
            );
        }
        authority.ensure_guard(guard)?;
        Ok(artifact)
    })
}
