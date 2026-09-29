//! Fixed inherited-descriptor coordinates shared by guest activation and the
//! protected supervisor. This is a pure protocol mapping, not descriptor I/O.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result, ensure};

use crate::{ExternalGuestInputProjection, GuestMountContentAuthority, MAX_GUEST_INPUTS};

pub const SUPERVISOR_BOOTSTRAP_FD: u32 = 50;
pub const SUPERVISOR_STATE_ROOT_FD: u32 = 51;
pub const SUPERVISOR_CANDIDATE_RUNTIME_FD: u32 = 52;
pub const SUPERVISOR_PRIVATE_PARENT_FD: u32 = 53;
pub const SUPERVISOR_LAUNCHER_FD: u32 = 54;
pub const SUPERVISOR_WORKSPACE_OUTPUT_FD: u32 = 55;
// Activation consumes the base transfer before the supervisor starts. The
// retained projection still requires a unique descriptor coordinate, but this
// slot is deliberately not an inherited supervisor authority.
pub const SUPERVISOR_CONSUMED_BASE_SNAPSHOT_FD: u32 = 56;
/// Exact executable source used by Lillux for the supervisor image. The
/// supervisor does not adopt it as an input after exec, but the outer owner
/// must include it in the complete exec-time inheritance proof.
pub const SUPERVISOR_EXECUTABLE_FD: u32 = 57;
/// Sealed one-way launch record delivered only after the stage was imported
/// and the outer owner committed its exact mounted-launch intent.
pub const SUPERVISOR_LAUNCH_INTENT_FD: u32 = 58;
pub const SUPERVISOR_STAGE_MOUNT_DESTINATION: &str = "/ryeos/guest-stage";
/// Only non-source controls may cross the held sealed-source launch. Source
/// content is opened from the sole read-only stage mount after target exec.
pub const SUPERVISOR_MOUNTED_CONTROL_DESCRIPTORS: [u32; 5] = [
    SUPERVISOR_BOOTSTRAP_FD,
    SUPERVISOR_STATE_ROOT_FD,
    SUPERVISOR_CANDIDATE_RUNTIME_FD,
    SUPERVISOR_PRIVATE_PARENT_FD,
    SUPERVISOR_LAUNCH_INTENT_FD,
];
pub const SUPERVISOR_RUNTIME_MOUNT_FD_BASE: u32 = 64;
pub const SUPERVISOR_CONTENT_RECORD_FD_BASE: u32 =
    SUPERVISOR_RUNTIME_MOUNT_FD_BASE + MAX_GUEST_INPUTS as u32;

/// Complete inherited descriptor coordinates for one supervisor launch. The
/// consumed base-snapshot coordinate is retained only in the rebound semantic
/// projection and is never included in `inherited_descriptors`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSupervisorDescriptorPlan {
    pub execution_inputs: ExternalGuestInputProjection,
    pub inherited_descriptors: Vec<u32>,
    pub runtime_mount_descriptors: Vec<u32>,
    pub content_record_descriptors: Vec<u32>,
}

/// Derive the one fixed mapping that both the guest owner and supervisor must
/// use. This does not open, inspect, or grant any descriptor authority.
pub fn fixed_guest_supervisor_descriptor_plan(
    inputs: &ExternalGuestInputProjection,
) -> Result<GuestSupervisorDescriptorPlan> {
    let mut execution_inputs = inputs.clone();
    rebind_fixed_guest_descriptors(&mut execution_inputs)?;
    let runtime_mount_descriptors = execution_inputs
        .inputs
        .iter()
        .map(|input| input.descriptor)
        .collect::<Vec<_>>();
    let content_record_descriptors = execution_inputs
        .record_descriptors()
        .map(|(descriptor, _, _)| descriptor)
        .collect::<Vec<_>>();
    let mut inherited_descriptors = vec![
        SUPERVISOR_BOOTSTRAP_FD,
        SUPERVISOR_STATE_ROOT_FD,
        SUPERVISOR_CANDIDATE_RUNTIME_FD,
        SUPERVISOR_PRIVATE_PARENT_FD,
        SUPERVISOR_LAUNCHER_FD,
        SUPERVISOR_EXECUTABLE_FD,
    ];
    if execution_inputs.workspace_outputs.is_some() {
        inherited_descriptors.push(SUPERVISOR_WORKSPACE_OUTPUT_FD);
    }
    inherited_descriptors.extend(&runtime_mount_descriptors);
    inherited_descriptors.extend(&content_record_descriptors);
    let unique = inherited_descriptors
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    ensure!(
        unique.len() == inherited_descriptors.len()
            && !unique.contains(&SUPERVISOR_CONSUMED_BASE_SNAPSHOT_FD),
        "fixed supervisor descriptor plan contains a collision or consumed base authority"
    );
    Ok(GuestSupervisorDescriptorPlan {
        execution_inputs,
        inherited_descriptors,
        runtime_mount_descriptors,
        content_record_descriptors,
    })
}

/// Normalize transport-local coordinates to the supervisor's fixed slots.
/// Descriptor numbers are not part of guest-input semantic identity. Activation
/// consumed the base transfer before this process starts; all live authorities
/// are independently adopted and verified against these fixed coordinates.
pub fn rebind_fixed_guest_descriptors(inputs: &mut ExternalGuestInputProjection) -> Result<()> {
    let semantic_identity = inputs.identity_digest()?;
    inputs.base_snapshot.descriptor = SUPERVISOR_CONSUMED_BASE_SNAPSHOT_FD;
    if let Some(outputs) = inputs.workspace_outputs.as_mut() {
        outputs.descriptor = SUPERVISOR_WORKSPACE_OUTPUT_FD;
    }
    let mut record_index = 0u32;
    for (index, input) in inputs.inputs.iter_mut().enumerate() {
        input.descriptor = SUPERVISOR_RUNTIME_MOUNT_FD_BASE
            .checked_add(u32::try_from(index)?)
            .context("external supervisor runtime mount descriptor overflow")?;
        let mut next_record_descriptor = || -> Result<u32> {
            let descriptor = SUPERVISOR_CONTENT_RECORD_FD_BASE
                .checked_add(record_index)
                .context("external supervisor content record descriptor overflow")?;
            record_index = record_index
                .checked_add(1)
                .context("external supervisor content record count overflow")?;
            Ok(descriptor)
        };
        match &mut input.content_authority {
            GuestMountContentAuthority::ProductManifest {
                manifest_descriptor,
                ..
            } => *manifest_descriptor = next_record_descriptor()?,
            GuestMountContentAuthority::SourceClosure {
                binding_descriptor,
                manifest_descriptor,
                ..
            } => {
                *binding_descriptor = next_record_descriptor()?;
                *manifest_descriptor = next_record_descriptor()?;
            }
            GuestMountContentAuthority::RawFile { .. }
            | GuestMountContentAuthority::PrivateScratch { .. } => {}
        }
    }
    inputs.validate()?;
    ensure!(
        inputs.identity_digest()? == semantic_identity,
        "fixed supervisor descriptor binding changed guest-input semantic identity"
    );
    Ok(())
}

/// Rebind the supervisor's semantic projection to descriptors it opened from
/// an independently attested read-only staged mount. The base transfer was
/// consumed before supervisor entry, so its descriptor is an unused sentinel
/// chosen beyond every opened coordinate. This is a pure process-local
/// mapping: the caller must verify each opened authority against the retained
/// projection and the outer owner must attest the exact mounted stage.
pub fn rebind_opened_guest_descriptors(
    inputs: &mut ExternalGuestInputProjection,
    workspace_output: Option<u32>,
    runtime_mounts: &[u32],
    content_records: &[u32],
) -> Result<()> {
    inputs.validate()?;
    let identity = inputs.identity_digest()?;
    let mut rebound = inputs.clone();
    ensure!(
        workspace_output.is_some() == inputs.workspace_outputs.is_some()
            && runtime_mounts.len() == inputs.inputs.len()
            && content_records.len() == inputs.record_descriptors().count(),
        "opened supervisor descriptor inventory differs from guest projection"
    );
    if let (Some(output), Some(descriptor)) = (&mut rebound.workspace_outputs, workspace_output) {
        output.descriptor = descriptor;
    }
    let mut records = content_records.iter().copied();
    for (input, descriptor) in rebound
        .inputs
        .iter_mut()
        .zip(runtime_mounts.iter().copied())
    {
        input.descriptor = descriptor;
        match &mut input.content_authority {
            GuestMountContentAuthority::ProductManifest {
                manifest_descriptor,
                ..
            } => {
                *manifest_descriptor = records.next().context("opened product record is absent")?;
            }
            GuestMountContentAuthority::SourceClosure {
                binding_descriptor,
                manifest_descriptor,
                ..
            } => {
                *binding_descriptor = records.next().context("opened source binding is absent")?;
                *manifest_descriptor =
                    records.next().context("opened source manifest is absent")?;
            }
            GuestMountContentAuthority::RawFile { .. }
            | GuestMountContentAuthority::PrivateScratch { .. } => {}
        }
    }
    ensure!(
        records.next().is_none(),
        "opened supervisor content record is extra"
    );
    let highest = workspace_output
        .into_iter()
        .chain(runtime_mounts.iter().copied())
        .chain(content_records.iter().copied())
        .max()
        .context("opened supervisor descriptor inventory is empty")?;
    rebound.base_snapshot.descriptor = highest
        .checked_add(1)
        .context("consumed guest base descriptor sentinel overflow")?;
    rebound.validate()?;
    ensure!(
        rebound.identity_digest()? == identity,
        "opened supervisor descriptor binding changed guest-input identity"
    );
    *inputs = rebound;
    Ok(())
}
