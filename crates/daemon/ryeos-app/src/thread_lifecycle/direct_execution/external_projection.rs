//! Mechanical guest projection of the existing retained ordinary plan.
//!
//! This is not allocation admission. The placement owner must supply the born
//! thread's authenticated closure/artifact, B-owned protocol and exact guest
//! inputs, then independently qualify the endpoint and check its live claim.
//! Neither this compiler nor the serialized projection grants provider contact.

#[cfg(test)]
mod tests;

mod project_file;
pub use project_file::{PreparedExternalDirectProjectFile, prepare_external_direct_project_file};

use std::collections::BTreeMap;
use std::path::Path;

use crate::env_contract::{EnvBinding, EnvContractBuilder, EnvSourceDetail};
use anyhow::{Context as _, Result, bail, ensure};
use ryeos_engine::contracts::{
    ExecutionEndpointRequirement, ExternalEndpointBindingIdentity, PlanArgument, PlanNode,
};
use ryeos_engine::isolation::{
    IsolationFilesystemAuthorityCeiling, IsolationNetworkAuthorityCeiling,
};
use ryeos_engine::protocol_vocabulary::{CallbackChannel, EnvInjectionSource, StdoutMode};
use ryeos_engine::protocols::VerifiedProtocol;
use ryeos_external_execution_contract::{
    ExternalExecutionMode, ExternalGuestInputProjection, GuestMountContentAuthority, GuestMountRole,
};
use ryeos_state::external_execution::admission::{
    AdmittedExternalDirectProgram, ExternalDirectCommandProjection,
    ExternalDirectNativeRequirements, ExternalDirectSealedInput,
};
use ryeos_state::objects::{
    AdmittedExecutionClosure, AdmittedLaunchArtifactIdentity, AdmittedLaunchCapsule,
    EnvironmentAuthority, ExecutionProjectAuthority, PinnedProjectRealization,
};
use ryeos_state::source_verification::VerifiedAdmittedSourceRecords;

/// One-use compiler evidence, not a serializable launch capability. The exact
/// captured launch claim must still be current under the allocation transaction.
/// Retained input lifelines and native readiness remain separate owners.
pub(crate) struct CompiledExternalDirectProgram {
    program: AdmittedExternalDirectProgram,
    capsule_hash: String,
    thread_id: String,
    chain_root_id: String,
    launch_owner: crate::runtime_db::LaunchOwner,
}

impl CompiledExternalDirectProgram {
    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn chain_root_id(&self) -> &str {
        &self.chain_root_id
    }

    pub(crate) fn capsule_hash(&self) -> &str {
        &self.capsule_hash
    }

    pub(crate) fn launch_owner(&self) -> &crate::runtime_db::LaunchOwner {
        &self.launch_owner
    }

    pub(crate) fn program(&self) -> &AdmittedExternalDirectProgram {
        &self.program
    }

    pub(crate) fn verify_reservation(
        &self,
        reservation: &crate::runtime_db::external_execution::ExternalAllocationReservation,
    ) -> Result<()> {
        use crate::runtime_db::external_execution::ExternalAllocationOwner;
        let ExternalAllocationOwner::DirectThread {
            chain_root_id,
            launch_owner,
            program,
        } = &reservation.owner
        else {
            bail!("direct compiler proof cannot authorize a session reservation");
        };
        ensure!(
            reservation.placement_thread_id == self.thread_id
                && reservation.admitted_capsule_hash == self.capsule_hash
                && chain_root_id == &self.chain_root_id
                && launch_owner == &self.launch_owner
                && program == &self.program,
            "direct compiler proof changed its exact born launch authority"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn external_direct_program_test_fixture() -> AdmittedExternalDirectProgram {
    tests::program_fixture()
}

/// Recover the exact endpoint selected by the sealed ordinary execution plan.
/// This authenticates neither the born thread nor its current launch claim;
/// those remain the StateStore allocation owner's responsibility.
pub(crate) fn retained_external_direct_endpoint(
    closure: &AdmittedExecutionClosure,
    artifact: &AdmittedLaunchArtifactIdentity,
) -> Result<ExternalEndpointBindingIdentity> {
    closure.validate()?;
    artifact.validate()?;
    let plan = super::decode_retained_direct_plan(closure, artifact)?;
    plan.validate_endpoint_for_sealing()?;
    ensure!(
        plan.subprocess_limits.is_none(),
        "external direct native subprocess limits require admitted current guest support before provider contact"
    );
    ensure!(
        matches!(
            plan.endpoint_requirement,
            ExecutionEndpointRequirement::External { .. }
        ),
        "external direct allocation requires a sealed external endpoint"
    );
    plan.external_endpoint_binding
        .context("external direct endpoint has not been sealed")
}

/// Classify the authenticated ordinary closure even before an allocation row
/// exists. Missing operational state must not exempt external work from its
/// terminal-evidence checks or turn it into local execution.
pub fn capsule_requires_external_direct(capsule: &AdmittedLaunchCapsule) -> Result<bool> {
    closure_requires_external_direct(&capsule.execution_closure, &capsule.artifact_identity)
}

fn closure_requires_external_direct(
    closure: &AdmittedExecutionClosure,
    artifact: &AdmittedLaunchArtifactIdentity,
) -> Result<bool> {
    let AdmittedExecutionClosure::DirectItemExecutor { execution_plan, .. } = closure else {
        return Ok(false);
    };
    let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        execution_plan_hash,
        ..
    } = artifact
    else {
        bail!("direct endpoint classification requires a direct artifact identity");
    };
    ensure!(
        lillux::sha256_hex(lillux::canonical_json(execution_plan)?.as_bytes())
            == *execution_plan_hash,
        "direct endpoint classification changed its retained plan hash"
    );
    let plan: ryeos_engine::contracts::ExecutionPlan =
        serde_json::from_value(execution_plan.clone())
            .context("decode retained direct endpoint for classification")?;
    plan.validate_endpoint_for_sealing()?;
    if matches!(
        plan.endpoint_requirement,
        ExecutionEndpointRequirement::Local {}
    ) {
        // Classification must not demand restart-recoverable executable bytes
        // from local NodePolicy work. It grants no recovery/spawn authority.
        return Ok(false);
    }
    // External finalization retains the full executable/runtime identity join.
    super::decode_retained_direct_plan(closure, artifact)?;
    Ok(true)
}

/// Rejoin retained compiler output to its original capsule. This is not proof
/// that arbitrary serialized arguments were compiled: first admission still
/// consumes `CompiledExternalDirectProgram`, and recovery uses its immutable row.
pub(crate) fn validate_retained_external_direct_program(
    capsule: &AdmittedLaunchCapsule,
    program: &AdmittedExternalDirectProgram,
) -> Result<ExternalEndpointBindingIdentity> {
    capsule.validate()?;
    program.validate()?;
    let endpoint =
        retained_external_direct_endpoint(&capsule.execution_closure, &capsule.artifact_identity)?;
    let AdmittedExecutionClosure::DirectItemExecutor { command, .. } = &capsule.execution_closure
    else {
        unreachable!("retained direct endpoint checked closure")
    };
    let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        execution_plan_hash,
        ..
    } = &capsule.artifact_identity
    else {
        bail!("external direct capsule lost its ordinary artifact")
    };
    ensure!(
        program.execution_closure_digest()
            == ryeos_state::objects::canonical_value_digest(&serde_json::to_value(
                &capsule.execution_closure
            )?)?
            && program.execution_plan_hash() == execution_plan_hash
            && program.command() == command
            && program.projection().endpoint_binding_id == endpoint.binding_id
            && program.projection().endpoint_binding_digest == endpoint.binding_digest,
        "external direct retained program changed its ordinary execution closure or endpoint"
    );
    Ok(endpoint)
}

pub(crate) fn compile_external_direct_program(
    capsule: &AdmittedLaunchCapsule,
    protocol: &VerifiedProtocol,
    thread_id: &str,
    chain_root_id: &str,
    inputs: &ExternalGuestInputProjection,
    source: Option<&VerifiedAdmittedSourceRecords>,
    launch_owner: crate::runtime_db::LaunchOwner,
    project_file: Option<&PreparedExternalDirectProjectFile>,
) -> Result<CompiledExternalDirectProgram> {
    // The placement owner authenticates the retained capsule and born thread;
    // this join also prevents the evaluator source being selected from C.
    capsule.validate()?;
    ensure!(
        launch_owner.thread_id == thread_id,
        "direct compiler launch owner changed its thread"
    );
    validate_external_direct_project_inputs(&capsule.project_authority, inputs)?;
    validate_direct_capsule_source(capsule, source)?;
    if let Some(proof) = project_file {
        proof.require_capsule(capsule)?;
    }
    let program = compile_external_direct_program_parts_with_project_file(
        &capsule.execution_closure,
        &capsule.artifact_identity,
        protocol,
        thread_id,
        chain_root_id,
        inputs,
        source,
        project_file,
    )?;
    Ok(CompiledExternalDirectProgram {
        program,
        capsule_hash: capsule.content_hash()?,
        thread_id: thread_id.to_owned(),
        chain_root_id: chain_root_id.to_owned(),
        launch_owner,
    })
}

fn validate_direct_capsule_source(
    capsule: &AdmittedLaunchCapsule,
    source: Option<&VerifiedAdmittedSourceRecords>,
) -> Result<()> {
    let projection = AdmittedLaunchCapsule::source_projection_in_program(&capsule.exact_program)?;
    match (source, projection.as_ref()) {
        (None, None) => Ok(()),
        (Some(records), Some(projection)) => records.validate_projection(projection),
        _ => bail!("external direct source presence differs from its retained capsule"),
    }
}

/// Derive input environment values using the same retained-plan/protocol owner
/// as the final compiler. No host environment, live project or caller-authored
/// search path enters this projection. The workspace view is selected from
/// the same exact plan; this is not born-thread/contact proof.
pub fn external_direct_environment(
    capsule: &AdmittedLaunchCapsule,
    protocol: &VerifiedProtocol,
    thread_id: &str,
    chain_root_id: &str,
    source: Option<&VerifiedAdmittedSourceRecords>,
) -> Result<(
    BTreeMap<String, String>,
    Vec<String>,
    ryeos_external_execution_contract::GuestWorkspaceView,
)> {
    capsule.validate()?;
    validate_external_direct_project_authority(&capsule.project_authority)?;
    validate_direct_capsule_source(capsule, source)?;
    let (plan, retained) = retained_external_direct_plan(
        &capsule.execution_closure,
        &capsule.artifact_identity,
        protocol,
    )?;
    let spec = ordinary_direct_subprocess(&plan)?;
    let environment =
        compile_direct_environment(spec, &retained, thread_id, chain_root_id, source)?;
    let search = environment
        .get("PATH")
        .map(|path| path.split(':').map(str::to_owned).collect())
        .unwrap_or_default();
    Ok((environment, search, direct_workspace_view(&plan)?))
}

fn direct_workspace_view(
    plan: &ryeos_engine::contracts::ExecutionPlan,
) -> Result<ryeos_external_execution_contract::GuestWorkspaceView> {
    use ryeos_external_execution_contract::GuestWorkspaceView;
    match plan.filesystem_authority_ceiling {
        IsolationFilesystemAuthorityCeiling::CapturedExecution => {
            Ok(GuestWorkspaceView::FullSnapshot)
        }
        IsolationFilesystemAuthorityCeiling::CapturedInputs => {
            Ok(GuestWorkspaceView::CapturedInputs)
        }
        IsolationFilesystemAuthorityCeiling::NodePolicy => {
            bail!("external direct workspace cannot inherit node filesystem authority")
        }
    }
}

fn direct_target_limits(
    plan: &ryeos_engine::contracts::ExecutionPlan,
) -> Result<Option<ryeos_state::external_execution::admission::ExternalNativeProcessLimits>> {
    let target_limits = plan
        .subprocess_limits
        .filter(|limits| {
            limits.max_open_files.is_some()
                || limits.max_address_space_bytes.is_some()
                || limits.max_cpu_seconds.is_some()
                || limits.max_processes.is_some()
                || limits.deny_process_creation
        })
        .map(
            |limits| ryeos_state::external_execution::admission::ExternalNativeProcessLimits {
                max_open_files: limits.max_open_files,
                max_address_space_bytes: limits.max_address_space_bytes,
                max_cpu_seconds: limits.max_cpu_seconds,
                max_processes: limits.max_processes,
                deny_process_creation: limits.deny_process_creation,
            },
        );
    if let Some(limits) = target_limits {
        limits.validate()?;
    }
    Ok(target_limits)
}

pub(super) fn required_direct_feature_claims(
    plan: &ryeos_engine::contracts::ExecutionPlan,
) -> Result<Vec<&'static str>> {
    use ryeos_external_execution_contract::{
        CAPTURED_INPUTS_SUPPORT_CLAIM, GuestWorkspaceView, TARGET_LIMITS_SUPPORT_CLAIM,
    };
    let mut required = Vec::new();
    if direct_workspace_view(plan)? == GuestWorkspaceView::CapturedInputs {
        required.push(CAPTURED_INPUTS_SUPPORT_CLAIM);
    }
    if direct_target_limits(plan)?.is_some() {
        required.push(TARGET_LIMITS_SUPPORT_CLAIM);
    }
    Ok(required)
}

/// Join the guest's C filesystem to the retained launch, independently of the
/// B-owned evaluator source. This checks identity, not descriptor redemption or
/// the placement owner's authenticated born-thread allocation authority.
fn validate_external_direct_project_inputs(
    authority: &ExecutionProjectAuthority,
    inputs: &ExternalGuestInputProjection,
) -> Result<()> {
    let snapshot_hash = validate_external_direct_project_authority(authority)?;
    ensure!(
        snapshot_hash == inputs.base_snapshot.snapshot_hash,
        "external direct guest snapshot differs from its retained project authority"
    );
    ensure!(
        inputs.workspace_outputs.is_none(),
        "external direct inputs cannot introduce workspace output authority"
    );
    Ok(())
}

/// Shared by prebirth admission and the born-capsule compiler. This establishes
/// the supported project authority, not a guest filesystem or materialization.
pub(super) fn validate_external_direct_project_authority(
    authority: &ExecutionProjectAuthority,
) -> Result<&str> {
    authority.validate()?;
    let ExecutionProjectAuthority::PinnedGeneration {
        snapshot_hash,
        realization: PinnedProjectRealization::ReadOnly,
        workspace_outputs: None,
        environment: EnvironmentAuthority::None,
        ..
    } = authority
    else {
        bail!(
            "external direct execution requires an immutable pinned generation without environment or output authority"
        );
    };
    Ok(snapshot_hash)
}

/// The finite ordinary-program contract supported by the direct guest. Run it
/// before birth and again over the retained plan; do not silently omit features
/// the guest projection cannot implement. Host-native readiness is separate.
pub(super) fn validate_external_direct_plan(
    plan: &ryeos_engine::contracts::ExecutionPlan,
    protocol: &ryeos_engine::protocols::ProtocolDescriptor,
) -> Result<()> {
    plan.validate_endpoint_for_sealing()?;
    ensure!(
        plan.subprocess_limits.is_none(),
        "external direct native subprocess limits require admitted current guest support before provider contact"
    );
    ensure!(
        plan.filesystem_authority_ceiling == IsolationFilesystemAuthorityCeiling::CapturedExecution,
        "external direct execution requires captured filesystem and isolated network ceilings"
    );
    validate_external_direct_plan_semantics(plan, protocol)
}

/// Validate the projection's finite mechanical meaning, independently of
/// ordinary guest support admission. This grants no allocation, dispatch,
/// qualification or fixture provenance. A mechanism preparation owner must
/// authenticate its original root/purpose and actual fixture closure before
/// using these semantics; ordinary callers retain the support gates above.
pub(super) fn validate_external_direct_plan_semantics(
    plan: &ryeos_engine::contracts::ExecutionPlan,
    protocol: &ryeos_engine::protocols::ProtocolDescriptor,
) -> Result<()> {
    plan.validate_endpoint_for_sealing()?;
    if let Some(limits) = plan.subprocess_limits {
        limits.validate()?;
    }
    direct_target_limits(plan)?;

    ensure!(
        matches!(
            plan.endpoint_requirement,
            ExecutionEndpointRequirement::External { .. }
        ),
        "local execution cannot be projected as an external direct command"
    );
    ensure!(
        protocol.callback_channel == CallbackChannel::None
            && protocol.session.is_none()
            && protocol.stdout.mode == StdoutMode::Terminal,
        "external direct execution requires callback-free one-shot terminal output"
    );
    ensure!(
        protocol.env_injections.iter().all(|injection| matches!(
            injection.source,
            EnvInjectionSource::ThreadId | EnvInjectionSource::ProjectPath
        )),
        "external direct protocol requests unsupported controller authority"
    );
    ensure!(
        matches!(
            plan.filesystem_authority_ceiling,
            IsolationFilesystemAuthorityCeiling::CapturedExecution
                | IsolationFilesystemAuthorityCeiling::CapturedInputs
        ) && plan.network_authority_ceiling == IsolationNetworkAuthorityCeiling::Isolated,
        "external direct execution requires captured filesystem and isolated network ceilings"
    );
    ensure!(
        !plan.capabilities.requires_model
            && !plan.capabilities.requires_network
            && plan.capabilities.custom.is_empty()
            && plan.materialization_requirements.is_empty(),
        "external direct projection cannot discard model, network or materialization requirements"
    );
    let target = plan
        .target_requirement
        .as_ref()
        .context("external direct execution needs an explicit guest target")?;
    ensure!(
        target.os == "linux" && target.resources.is_empty(),
        "external direct execution requires an explicit Linux target without device resources"
    );
    ExternalDirectNativeRequirements::LinuxIsolatedReadOnly {
        required_arch: target.arch.clone(),
    }
    .validate()?;
    let spec = ordinary_direct_subprocess(plan)?;
    ensure!(
        spec.execution.native_async.is_none() && spec.execution.native_resume.is_none(),
        "external direct projection cannot discard native async or resume semantics"
    );
    ensure!(
        (1..=ryeos_state::external_execution::admission::MAX_EXTERNAL_DIRECT_TIMEOUT_SECONDS)
            .contains(&spec.timeout_secs),
        "external direct timeout exceeds bounds"
    );
    Ok(())
}

/// Preserve the ordinary builder's terminal topology rather than projecting
/// an arbitrary executable subgraph or silently dropping trailing work.
pub(super) fn ordinary_direct_subprocess(
    plan: &ryeos_engine::contracts::ExecutionPlan,
) -> Result<&ryeos_engine::contracts::PlanSubprocessSpec> {
    let [
        PlanNode::DispatchSubprocess { id, spec, .. },
        PlanNode::Complete { id: complete_id },
    ] = plan.nodes.as_slice()
    else {
        bail!(
            "external direct projection requires exactly one ordinary subprocess followed by Complete"
        );
    };
    ensure!(
        id != complete_id,
        "external direct plan has duplicate node identities"
    );
    ensure!(
        *id == plan.entrypoint,
        "external direct plan entrypoint changed"
    );
    Ok(spec)
}

fn retained_external_direct_plan(
    closure: &AdmittedExecutionClosure,
    artifact: &AdmittedLaunchArtifactIdentity,
    protocol: &VerifiedProtocol,
) -> Result<(
    ryeos_engine::contracts::ExecutionPlan,
    ryeos_engine::protocols::ProtocolDescriptor,
)> {
    let (mut plan, retained) = retained_direct_plan_and_protocol(closure, artifact, protocol)?;
    let AdmittedExecutionClosure::DirectItemExecutor {
        admitted_project_root,
        ..
    } = closure
    else {
        unreachable!("retained direct plan checked above")
    };
    validate_external_direct_plan(&plan, &retained)?;
    ensure!(
        admitted_project_root.is_some(),
        "external direct execution requires a retained project view"
    );
    super::relocate_admitted_direct_plan(
        &mut plan,
        admitted_project_root.as_deref(),
        Some(Path::new("/workspace")),
    )?;
    Ok((plan, retained))
}

fn retained_direct_plan_and_protocol(
    closure: &AdmittedExecutionClosure,
    artifact: &AdmittedLaunchArtifactIdentity,
    protocol: &VerifiedProtocol,
) -> Result<(
    ryeos_engine::contracts::ExecutionPlan,
    ryeos_engine::protocols::ProtocolDescriptor,
)> {
    closure.validate()?;
    artifact.validate()?;
    let plan = super::decode_retained_direct_plan(closure, artifact)?;
    let AdmittedExecutionClosure::DirectItemExecutor {
        protocol_descriptor_document,
        ..
    } = closure
    else {
        unreachable!("retained direct plan checked above")
    };
    let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        protocol_ref,
        protocol_content_hash,
        protocol_signer_fingerprint,
        ..
    } = artifact
    else {
        unreachable!("retained direct artifact checked above")
    };
    let body = lillux::signature::strip_signature_lines(protocol_descriptor_document);
    ensure!(
        protocol.canonical_ref == *protocol_ref
            && protocol.raw_content_digest == *protocol_content_hash
            && protocol.signer_fingerprint == *protocol_signer_fingerprint
            && lillux::signature::content_hash(&body) == *protocol_content_hash,
        "external direct protocol differs from its retained authority"
    );
    // The caller supplies an already verified protocol; rejoin its interpreted
    // descriptor too, without resolving a live registry or re-opening a file.
    let retained: ryeos_engine::protocols::ProtocolDescriptor = serde_yaml::from_str(&body)?;
    ensure!(
        serde_json::to_value(&retained)? == serde_json::to_value(&protocol.descriptor)?,
        "external direct protocol interpretation changed its retained document"
    );
    ryeos_engine::protocols::validate_admitted_protocol_descriptor(protocol_ref, &retained)?;
    Ok((plan, retained))
}

#[cfg(test)]
fn compile_external_direct_program_parts(
    closure: &AdmittedExecutionClosure,
    artifact: &AdmittedLaunchArtifactIdentity,
    protocol: &VerifiedProtocol,
    thread_id: &str,
    chain_root_id: &str,
    inputs: &ExternalGuestInputProjection,
    source: Option<&VerifiedAdmittedSourceRecords>,
) -> Result<AdmittedExternalDirectProgram> {
    compile_external_direct_program_parts_with_project_file(
        closure,
        artifact,
        protocol,
        thread_id,
        chain_root_id,
        inputs,
        source,
        None,
    )
}

fn compile_external_direct_program_parts_with_project_file(
    closure: &AdmittedExecutionClosure,
    artifact: &AdmittedLaunchArtifactIdentity,
    protocol: &VerifiedProtocol,
    thread_id: &str,
    chain_root_id: &str,
    inputs: &ExternalGuestInputProjection,
    source: Option<&VerifiedAdmittedSourceRecords>,
    project_file: Option<&PreparedExternalDirectProjectFile>,
) -> Result<AdmittedExternalDirectProgram> {
    let (plan, retained) = retained_external_direct_plan(closure, artifact, protocol)?;
    compile_direct_program_from_plan(
        &plan,
        &retained,
        closure,
        artifact,
        thread_id,
        chain_root_id,
        inputs,
        source,
        project_file,
    )
}

#[allow(clippy::too_many_arguments)]
fn compile_direct_program_from_plan(
    plan: &ryeos_engine::contracts::ExecutionPlan,
    retained: &ryeos_engine::protocols::ProtocolDescriptor,
    closure: &AdmittedExecutionClosure,
    artifact: &AdmittedLaunchArtifactIdentity,
    thread_id: &str,
    chain_root_id: &str,
    inputs: &ExternalGuestInputProjection,
    source: Option<&VerifiedAdmittedSourceRecords>,
    project_file: Option<&PreparedExternalDirectProjectFile>,
) -> Result<AdmittedExternalDirectProgram> {
    let target = plan
        .target_requirement
        .as_ref()
        .expect("validated direct target");
    let required_arch = target.arch.clone();
    let ExecutionEndpointRequirement::External {
        stdout_max_bytes,
        stderr_max_bytes,
        ..
    } = plan.endpoint_requirement
    else {
        bail!("local execution cannot be projected as an external direct command");
    };
    let endpoint = plan
        .external_endpoint_binding
        .clone()
        .context("external direct endpoint has not been sealed")?;
    let spec = ordinary_direct_subprocess(plan)?;
    inputs.validate()?;
    let project_file_destination = project_file::require_inputs(plan, inputs, project_file)?;
    ensure!(
        inputs.workspace_view == direct_workspace_view(plan)?,
        "external direct workspace view differs from its retained plan ceiling"
    );
    let source_mounts = inputs
        .inputs
        .iter()
        .filter(|input| input.role == GuestMountRole::Source)
        .collect::<Vec<_>>();
    let source_entry = match source {
        None => {
            ensure!(
                source_mounts.is_empty(),
                "external direct inputs contain unbound source"
            );
            None
        }
        Some(records) => {
            let [mount] = source_mounts.as_slice() else {
                bail!("external direct source requires exactly one retained mount");
            };
            let GuestMountContentAuthority::SourceClosure {
                binding_hash,
                manifest_hash,
                binding_bytes,
                manifest_bytes,
                ..
            } = &mount.content_authority
            else {
                bail!("external direct source mount lost its source authority");
            };
            ensure!(
                binding_hash == records.binding_hash()
                    && manifest_hash == records.content_manifest_hash()
                    && mount.authority_id == records.binding_hash()
                    && Path::new(&mount.destination) == records.runtime_destination()
                    && mount.bytes == records.manifest().totals.total_bytes
                    && *binding_bytes
                        == lillux::canonical_json(&records.binding().to_value()?)?.len() as u64
                    && *manifest_bytes
                        == lillux::canonical_json(&records.manifest().to_value()?)?.len() as u64,
                "external direct source mount differs from retained B authority"
            );
            Some(records.runtime_entry_path())
        }
    };
    let source_arguments = spec
        .args
        .iter()
        .filter(|argument| matches!(argument, PlanArgument::AdmittedSourceEntry))
        .count();
    ensure!(
        source_arguments == usize::from(source.is_some()),
        "external direct source requires one typed entry and no unbound source argument"
    );
    let arguments = spec
        .args
        .iter()
        .map(|argument| match argument {
            PlanArgument::Literal { value } => Ok(value.clone()),
            PlanArgument::AdmittedSourceEntry => Ok(source_entry
                .as_ref()
                .context("external direct source is absent")?
                .to_str()
                .context("external direct source entry is not UTF-8")?
                .to_owned()),
            PlanArgument::AdmittedExecutionProjectFile { .. } => Ok(project_file_destination
                .as_ref()
                .context("external execution project file has no bound input")?
                .clone()),
            PlanArgument::AdmittedSourceMember { .. } => {
                bail!("external direct source members require an admitted guest binding")
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let environment = compile_direct_environment(spec, retained, thread_id, chain_root_id, source)?;
    match environment.get("PATH") {
        Some(path) => ensure!(
            path == &inputs.executable_search.join(":"),
            "external direct PATH differs from its admitted immutable executable search"
        ),
        None => ensure!(
            inputs.executable_search.is_empty(),
            "external direct executable search has no compiled PATH binding"
        ),
    }
    ensure!(
        environment == inputs.environment,
        "external direct guest environment differs from its compiled ordinary plan"
    );
    let stdin = spec
        .stdin
        .as_ref()
        .map(|value| value.materialize().map_err(anyhow::Error::msg))
        .transpose()?
        .unwrap_or_default();
    let requested = plan.subprocess_limits;
    let target_limits = direct_target_limits(plan)?;
    let stdout_max_bytes = requested
        .and_then(|limits| limits.max_stdout_bytes)
        .map_or(stdout_max_bytes, |limit| stdout_max_bytes.min(limit));
    let stderr_max_bytes = requested
        .and_then(|limits| limits.max_stderr_bytes)
        .map_or(stderr_max_bytes, |limit| stderr_max_bytes.min(limit));
    let projection = ExternalDirectCommandProjection {
        workspace_view: direct_workspace_view(plan)?,
        target_limits,
        argv0: AdmittedExternalDirectProgram::guest_executable_for_command(match closure {
            AdmittedExecutionClosure::DirectItemExecutor { command, .. } => command,
            _ => bail!("external direct projection lost its ordinary closure"),
        })?,
        arguments,
        cwd: spec
            .cwd
            .as_ref()
            .context("external direct command has no exact cwd")?
            .to_str()
            .context("external direct cwd is not UTF-8")?
            .to_owned(),
        environment,
        stdin: ExternalDirectSealedInput::from_bytes(stdin.as_bytes())?,
        timeout_seconds: spec.timeout_secs,
        endpoint_binding_id: endpoint.binding_id,
        endpoint_binding_digest: endpoint.binding_digest,
        execution_mode: ExternalExecutionMode::DirectCommand {
            stdout_max_bytes,
            stderr_max_bytes,
        },
        native: ExternalDirectNativeRequirements::LinuxIsolatedReadOnly { required_arch },
    };
    AdmittedExternalDirectProgram::from_compiled_closure(closure, artifact, projection, inputs)
}

fn compile_direct_environment(
    spec: &ryeos_engine::contracts::PlanSubprocessSpec,
    retained: &ryeos_engine::protocols::ProtocolDescriptor,
    thread_id: &str,
    chain_root_id: &str,
    source: Option<&VerifiedAdmittedSourceRecords>,
) -> Result<BTreeMap<String, String>> {
    ensure!(
        !thread_id.is_empty()
            && thread_id.len() <= 256
            && !thread_id.chars().any(char::is_control)
            && !chain_root_id.is_empty()
            && chain_root_id.len() <= 256
            && !chain_root_id.chars().any(char::is_control),
        "external direct thread coordinate is invalid"
    );
    // The same provenance/policy owner as ordinary spawn, with no inherited
    // host allowlist, daemon roots, or either vault in this guest projection.
    let mut environment = EnvContractBuilder::new()
        .with_typed_bindings([
            EnvBinding::new("RYEOS_THREAD_ID", thread_id, EnvSourceDetail::EnginePlanEnv),
            EnvBinding::new(
                "RYEOS_CHAIN_ROOT_ID",
                chain_root_id,
                EnvSourceDetail::EnginePlanEnv,
            ),
        ])?
        .with_typed_bindings(super::runtime_environment_bindings(spec))?;
    if let Some(records) = source {
        ensure!(
            !spec.env.contains_key("RYEOS_ADMITTED_SOURCE"),
            "external direct plan overrides protected source authority"
        );
        environment = environment.with_typed_bindings([EnvBinding::new(
            "RYEOS_ADMITTED_SOURCE",
            records.sealed_identity_env(),
            EnvSourceDetail::PerSpawnDaemon,
        )])?;
    }
    for injection in &retained.env_injections {
        let value = match injection.source {
            EnvInjectionSource::ThreadId => thread_id.to_owned(),
            EnvInjectionSource::ProjectPath => "/workspace".to_owned(),
            // No controller CAS, vault, callback, actor or state-root authority
            // is delivered implicitly by choosing an external placement.
            _ => bail!("external direct protocol requests unsupported controller authority"),
        };
        ensure!(
            !spec.env.contains_key(&injection.name),
            "external direct plan overrides a protocol-owned environment binding"
        );
        environment = environment.with_typed_bindings([EnvBinding::new(
            injection.name.clone(),
            value,
            EnvSourceDetail::ProtocolInjection {
                source: injection.source,
            },
        )])?;
    }
    Ok(environment.build().into_iter().collect())
}
