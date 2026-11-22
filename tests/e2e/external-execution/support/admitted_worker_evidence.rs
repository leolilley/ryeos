//! Test-only, non-circular Worker evidence from one signed source admission.
//! This does not qualify a product or issue external-execution claims.

use anyhow::Result;

pub type AdmittedWorkerEvidence =
    ryeos_app::source_closure_admission::AdmittedBundleStructuredWorkerProfile;

/// Capture the signed Worker once, then compile the full profile from that
/// capture's CAS blobs and logical entry. Do not read the live bundle a second
/// time or infer source coordinates from a prospective product result.
pub fn admit_worker(
    state: &ryeos_app::state::AppState,
    worker_ref: &str,
) -> Result<AdmittedWorkerEvidence> {
    ryeos_app::source_closure_admission::admit_bundle_structured_worker_profile(state, worker_ref)
}
