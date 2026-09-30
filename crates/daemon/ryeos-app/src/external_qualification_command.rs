//! Signed first Request selection for one qualification-only local Worker.
//!
//! This facade grants no send, contact, or claim. The caller receives only the
//! exact body selected by the accepted signed scenario while both the Q
//! occurrence gate and one checked Bundle generation remain held.

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;
use ryeos_state::objects::canonical_value_digest;
use serde_json::Value;

use crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot;
use crate::runtime_db::external_execution::ExternalAllocationOwner;
use crate::runtime_db::runtime_snapshot_qualification::SnapshotQualificationPhase;
use crate::state::AppState;

/// Supply the selected signed scenario's bounded body and its canonical
/// digest to one synchronous caller. The callback remains inside the Q
/// occurrence contact gate and checked Bundle generation, but must separately
/// obtain its one-use command journal authority before any Worker send.
pub fn with_signed_qualification_worker_request<T>(
    state: &AppState,
    placement: &str,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    binding_digest: &str,
    root: &PreparedExternalQualificationRoot,
    run: impl FnOnce(Value, String) -> Result<T>,
) -> Result<T> {
    crate::external_qualification_local::with_qualification_local_owner(
        state,
        placement,
        coordinate,
        binding_digest,
        root,
        |_| {
            state.engine.with_checked_bundle_generation(|generation| {
                let allocation = state
                    .state_store
                    .external_allocation(placement)?
                    .context("Q Worker request lost its allocation")?;
                let ExternalAllocationOwner::QualificationCandidate(owner) =
                    &allocation.reservation.owner
                else {
                    bail!("Q Worker request has no qualification owner");
                };
                ensure!(
                    owner.coordinate == *coordinate && owner.program == *root.program(),
                    "Q Worker request changed its accepted owner"
                );
                let qualification = state
                    .state_store
                    .snapshot_qualification_operation(&owner.qualification_operation_id)?
                    .context("Q Worker request lost its restored qualification")?;
                ensure!(
                    qualification.intent.operation_id == owner.qualification_operation_id
                        && qualification.intent.owner_principal == owner.operator_principal
                        && qualification.phase == SnapshotQualificationPhase::OccurrenceBound
                        && qualification.occurrence.as_ref()
                            == Some(&owner.qualification_occurrence),
                    "Q Worker request changed its restored occurrence"
                );
                let profile = state
                    .node_config
                    .runtime_snapshot_qualification
                    .iter()
                    .find(|binding| {
                        binding.digest() == qualification.intent.qualification_profile_digest
                    })
                    .context("Q Worker request lost its signed qualification profile")?;
                ensure!(
                    qualification.intent.verifier_artifact_hash == profile.verifier_artifact_hash(),
                    "Q Worker request changed its signed qualification verifier"
                );
                let selection = profile.consumer_verifier(coordinate)?;
                let purpose = root.purpose();
                purpose.validate_remote_consumer_coordinate(coordinate, selection)?;
                let source = purpose
                    .remote_verifier_sources
                    .get(&coordinate.scenario_id)
                    .context("Q Worker request has no accepted selected verifier source")?;
                ensure!(
                    source.scenario_source_digest == selection.scenario_source_digest
                        && source.executor.payload_blob_hash == selection.verifier_artifact_hash,
                    "Q Worker request differs from its selected signed verifier source"
                );
                // This rechecks the current policy and all Worker definitions
                // against the accepted root in this same checked generation.
                root.current_local_worker_resolution(generation)?;
                let scenario = purpose
                    .policy_source
                    .policy
                    .producer_scenarios
                    .get(&coordinate.scenario_id)
                    .context("Q Worker request has no selected signed scenario")?;
                ensure!(
                    scenario.remote_verifier.as_ref() == Some(&source.selection),
                    "Q Worker request changed its signed scenario verifier"
                );
                let request = scenario
                    .consumer_worker_request
                    .as_ref()
                    .context("selected Q scenario has no signed Worker request")?;
                request.validate()?;
                let body = request.body.clone();
                let digest = canonical_value_digest(&body)?;
                run(body, digest)
            })
        },
    )
}
