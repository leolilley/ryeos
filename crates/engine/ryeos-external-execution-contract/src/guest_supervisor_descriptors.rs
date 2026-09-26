//! Fixed inherited-descriptor coordinates shared by guest activation and the
//! protected supervisor. This is a pure protocol mapping, not descriptor I/O.

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
pub const SUPERVISOR_RUNTIME_MOUNT_FD_BASE: u32 = 64;
pub const SUPERVISOR_CONTENT_RECORD_FD_BASE: u32 =
    SUPERVISOR_RUNTIME_MOUNT_FD_BASE + MAX_GUEST_INPUTS as u32;

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
