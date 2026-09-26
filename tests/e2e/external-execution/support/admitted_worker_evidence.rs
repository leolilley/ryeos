//! Test-only, non-circular Worker evidence from one signed source admission.
//! This does not qualify a product or issue external-execution claims.

use anyhow::{Context as _, Result, ensure};
use ryeos_engine::{canonical_ref::CanonicalRef, contracts::SubjectResolutionAuthority};
use ryeos_state::objects::{
    AdmittedStructuredSessionProfile, EffectiveSourceClosureProjection, SOURCE_CLOSURE_DERIVED_KEY,
};

pub struct AdmittedWorkerEvidence {
    pub source: EffectiveSourceClosureProjection,
    pub profile: AdmittedStructuredSessionProfile,
}

/// Capture the signed Worker once, then compile the full profile from that
/// capture's CAS blobs and logical entry. Do not read the live bundle a second
/// time or infer source coordinates from a prospective product result.
pub fn admit_worker(
    state: &ryeos_app::state::AppState,
    worker_ref: &str,
) -> Result<AdmittedWorkerEvidence> {
    let roots = state.engine.resolution_roots(None);
    let mut resolution =
        state
            .engine
            .effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
                item_ref: CanonicalRef::parse(worker_ref)?,
                expected_kind: Some("worker".into()),
                project_root: None,
                subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            })?;
    let captured = ryeos_app::source_closure_admission::admit_source_closure(
        state,
        &state.engine,
        "worker",
        &mut resolution,
        &roots,
        None,
        None,
    )?
    .context("signed Worker has no admitted source closure")?;
    let source = EffectiveSourceClosureProjection::from_value(
        resolution
            .composed
            .derived
            .get(SOURCE_CLOSURE_DERIVED_KEY)
            .context("source admission omitted its exact projection")?,
    )?;
    ensure!(
        captured.binding().digest()? == source.binding_hash
            && captured.manifest().digest()? == source.content_manifest_hash,
        "signed Worker source projection differs from its captured closure"
    );
    let profile_authority = captured.source_authority()?;
    let guard = profile_authority.acquire_shared_guard()?;
    profile_authority.ensure_guard(&guard)?;
    let profile = ryeos_app::source_closure_admission::compile_admitted_structured_worker_profile(
        &captured, &guard,
    )?;
    drop(guard);
    let publication = captured.into_publication();
    profile.validate()?;
    if let Some(publication) = publication {
        publication.publish()?;
    }
    Ok(AdmittedWorkerEvidence { source, profile })
}
