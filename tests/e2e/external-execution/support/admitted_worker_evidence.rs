//! Test-only, non-circular Worker evidence from one signed source admission.
//! This does not qualify a product or issue external-execution claims.

use anyhow::Result;
use ryeos_state::objects::{AdmittedStructuredSessionProfile, EffectiveSourceClosureProjection};

pub struct AdmittedWorkerEvidence {
    pub bundle_generation_identity: String,
    pub source: EffectiveSourceClosureProjection,
    pub profile: AdmittedStructuredSessionProfile,
    pub signed_effective_definition_digest: String,
    pub preselection_effective_definition_digest: String,
}

/// Capture the signed Worker once, then compile the full profile from that
/// capture's CAS blobs and logical entry. Do not read the live bundle a second
/// time or infer source coordinates from a prospective product result.
pub fn admit_worker(
    state: &ryeos_app::state::AppState,
    worker_ref: &str,
) -> Result<AdmittedWorkerEvidence> {
    let admitted = ryeos_app::source_closure_admission::admit_bundle_structured_worker_profile(
        state, worker_ref,
    )?;
    Ok(AdmittedWorkerEvidence {
        bundle_generation_identity: admitted.bundle_generation_identity,
        source: admitted.source,
        profile: admitted.profile,
        signed_effective_definition_digest: admitted.signed_effective_definition_digest,
        preselection_effective_definition_digest: admitted.preselection_effective_definition_digest,
    })
}
