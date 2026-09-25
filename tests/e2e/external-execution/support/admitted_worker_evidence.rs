//! Test-only, non-circular Worker evidence from one signed source admission.
//! This does not qualify a product or issue external-execution claims.

use anyhow::{Context as _, Result, ensure};
use ryeos_engine::{canonical_ref::CanonicalRef, contracts::SubjectResolutionAuthority};
use ryeos_state::objects::{
    AdmittedStructuredSessionProfile, EffectiveSourceClosureProjection, SOURCE_CLOSURE_DERIVED_KEY,
    SourceLogicalBinding,
};
use std::collections::BTreeMap;

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
    let entry = match &captured.binding().logical_binding {
        SourceLogicalBinding::Worker { entry, .. } => entry.clone(),
        _ => anyhow::bail!("admitted Worker has a non-Worker logical binding"),
    };
    let manifest = captured.manifest().clone();
    let publication = captured.into_publication();
    let authority = publication
        .as_ref()
        .map(|pending| pending.authority().try_clone())
        .transpose()?
        .unwrap_or(state.state_store.pinned_state_authority()?);
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let mut source_files = BTreeMap::new();
    for file in &manifest.entries {
        let bytes = cas
            .get_blob(&file.blob_hash)?
            .context("captured Worker source blob is absent from its CAS authority")?;
        source_files.insert(file.path.clone(), bytes);
    }
    let profile_bytes = source_files
        .get(&entry)
        .context("Worker logical entry is absent from its captured source")?;
    let profile = ryeos_engine::structured_session_profile::compile(profile_bytes, &source_files)?;
    profile.validate()?;
    drop(cas);
    drop(guard);
    if let Some(publication) = publication {
        publication.publish()?;
    }
    Ok(AdmittedWorkerEvidence { source, profile })
}
