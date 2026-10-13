//! Redeem immutable inputs for an ordinary externally placed command.
//!
//! No live project, worker capsule or candidate-export authority is consulted.
//! Environment/search values come from the app's retained ordinary-plan owner,
//! which independently rejoins the completed inputs before allocation. This
//! materialization owner grants no contact.

#[cfg(test)]
mod tests;

use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority;
use ryeos_external_execution_contract::{
    EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, ExternalGuestInputProjection, GuestBaseSnapshotInput,
};
use ryeos_state::objects::{
    AdmittedDirectCommandClosure, AdmittedExecutionClosure, AdmittedLaunchCapsule,
    EnvironmentAuthority, ExecutionProjectAuthority, PinnedProjectRealization,
};
use ryeos_state::source_verification::VerifiedAdmittedSourceRecords;

pub(crate) struct PreparedExternalDirectInputs {
    pub authority: ExternalGuestInputAuthority,
    /// Original verified B-source records for the app's ordinary-plan compiler.
    /// Their directory and generation lease are retained inside `authority`.
    pub source: Option<VerifiedAdmittedSourceRecords>,
    pub project_file:
        Option<std::sync::Arc<ryeos_app::thread_lifecycle::PreparedExternalDirectProjectFile>>,
}

pub(crate) fn prepare_external_direct_inputs(
    state: &ryeos_app::state::AppState,
    capsule: &AdmittedLaunchCapsule,
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    thread_id: &str,
    chain_root_id: &str,
    execution_materialization: Option<&ryeos_state::PinnedProjectMaterialization>,
) -> Result<PreparedExternalDirectInputs> {
    let sealed =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            capsule,
        )?;
    let resolution = sealed.admitted_effective_resolution()?;
    let ExecutionProjectAuthority::PinnedGeneration {
        snapshot_hash,
        realization: PinnedProjectRealization::ReadOnly,
        environment: EnvironmentAuthority::None,
        ..
    } = &capsule.project_authority
    else {
        anyhow::bail!("external direct inputs require the exact immutable pinned generation");
    };
    require_evaluator_execution_materialization(
        sealed.candidate_evaluation_authority(),
        snapshot_hash,
        execution_materialization,
    )?;
    let AdmittedExecutionClosure::DirectItemExecutor {
        command:
            AdmittedDirectCommandClosure::RealizationMember {
                realization_id,
                realization_manifest_hash,
                ..
            },
        ..
    } = &capsule.execution_closure
    else {
        anyhow::bail!("external direct inputs require an ordinary exact realization executable");
    };
    let project_file = ryeos_app::thread_lifecycle::prepare_external_direct_project_file(
        capsule,
        protocol,
        execution_materialization,
    )?
    .map(std::sync::Arc::new);
    let products = super::external_guest_inputs::prepare_product_inputs(
        state,
        resolution,
        Path::new("/workspace"),
        None,
    )?;
    ensure!(
        products.inputs.iter().any(|input| input.authority_id == *realization_id
            && matches!(&input.content_authority,
                ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest { manifest_hash, .. }
                if manifest_hash == realization_manifest_hash)),
        "external direct executable lost its exact realized input"
    );
    let source = super::source_closure::bind_external_source(state, resolution, capsule)?;
    let (environment, executable_search, workspace_view) =
        ryeos_app::thread_lifecycle::external_direct_environment(
            capsule,
            protocol,
            thread_id,
            chain_root_id,
            source.as_ref().map(|source| &source.records),
        )?;
    let state_authority = state.state_store.pinned_state_authority()?;
    let guard = state_authority.acquire_shared_guard()?;
    let base = ryeos_project_capture::prepare_project_snapshot_transfer(
        &state_authority,
        &guard,
        snapshot_hash,
    )
    .context("prepare exact external direct project transfer")?;
    drop(guard);
    let base_descriptor = base.descriptor();
    let mut projection = ExternalGuestInputProjection {
        workspace_view,
        schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
        base_snapshot: GuestBaseSnapshotInput {
            descriptor: base_descriptor
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            snapshot_hash: snapshot_hash.clone(),
            closure_digest: base.closure_digest().to_owned(),
            object_count: base.object_count(),
            blob_count: base.blob_count(),
            total_bytes: base.total_bytes(),
        },
        workspace_outputs: None,
        inputs: products.inputs,
        executable_search,
        environment,
    };
    let mut authorities = products.authorities;
    let mut records = products.manifest_authorities;
    let mut lifelines: Vec<Box<dyn Send + Sync>> = products
        .leases
        .into_iter()
        .map(|lease| Box::new(lease) as Box<dyn Send + Sync>)
        .collect();
    lifelines.push(Box::new(base));
    let source_records = if let Some(source) = source {
        projection.inputs.push(source.input);
        authorities.push(source.directory);
        records.extend(source.content_records);
        lifelines.push(Box::new(source.lifeline));
        Some(source.records)
    } else {
        None
    };
    if let Some(file) = &project_file {
        projection.inputs.push(file.input()?);
        authorities.push(file.input_authority());
        lifelines.push(Box::new(std::sync::Arc::clone(file)));
    }
    let authority = ExternalGuestInputAuthority::new(
        projection,
        base_descriptor,
        None,
        authorities,
        records,
        lifelines,
    )?;
    Ok(PreparedExternalDirectInputs {
        authority,
        source: source_records,
        project_file,
    })
}

/// Rejoin C execution custody independently of B definition/source authority.
fn require_evaluator_execution_materialization(
    evaluation: Option<&ryeos_app::thread_lifecycle::CandidateEvaluationAuthority>,
    snapshot_hash: &str,
    execution_materialization: Option<&ryeos_state::PinnedProjectMaterialization>,
) -> Result<()> {
    if let Some(evaluation) = evaluation {
        evaluation.validate()?;
        let materialization = execution_materialization
            .context("external evaluator inputs have no verified execution materialization")?;
        ensure!(
            evaluation.candidate_snapshot_hash == snapshot_hash
                && materialization.snapshot_hash() == snapshot_hash,
            "external evaluator execution materialization differs from sealed candidate snapshot"
        );
    }
    Ok(())
}
