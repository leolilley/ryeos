//! Recheck imported guest content against the retained activation projection.
//!
//! This is an observation over a private staged generation, not a Ready or
//! writer-exclusion claim. The caller must pass the independently retained
//! projection, own the stage exclusively, and repeat the checks at supervisor
//! adoption before authorizing execution.

use std::ffi::OsStr;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::{
    ExternalGuestInputProjection, GuestMountContentAuthority, GuestMountKind,
    GuestProductManifestKind,
};
use ryeos_external_execution_contract::guest_supervisor_descriptors::rebind_opened_guest_descriptors;

use crate::guest_staging::StagedGuestPackage;

/// Point-verified, still-open staged authorities. Private scratch has no
/// staged source and is represented by `None`;
/// the occurrence owner must create and verify that slot separately. These
/// handles do not exclude concurrent writers or authorize supervisor launch.
pub struct VerifiedGuestContentHandles {
    guest_input_identity: String,
    pub(crate) workspace_outputs: Option<lillux::InheritedDescriptorAuthority>,
    pub(crate) runtime_mounts: Vec<Option<lillux::InheritedDescriptorAuthority>>,
    pub(crate) content_records: Vec<lillux::InheritedDescriptorAuthority>,
}

/// Opened source content plus exact occurrence-owned scratch, rebound to the
/// descriptors the trusted supervisor actually holds. This is still content
/// custody, not source-mount or writer-exclusion evidence.
pub struct BoundGuestMountedContent {
    execution_inputs: ExternalGuestInputProjection,
    workspace_outputs: Option<lillux::InheritedDescriptorAuthority>,
    runtime_mounts: Vec<lillux::InheritedDescriptorAuthority>,
    content_records: Vec<lillux::InheritedDescriptorAuthority>,
}

impl BoundGuestMountedContent {
    pub fn into_parts(
        self,
    ) -> (
        ExternalGuestInputProjection,
        Option<lillux::InheritedDescriptorAuthority>,
        Vec<lillux::InheritedDescriptorAuthority>,
        Vec<lillux::InheritedDescriptorAuthority>,
    ) {
        (
            self.execution_inputs,
            self.workspace_outputs,
            self.runtime_mounts,
            self.content_records,
        )
    }
}

impl VerifiedGuestContentHandles {
    /// Join opened staged content with the exact private-scratch slots and
    /// rebind only process-local descriptor coordinates. The outer owner must
    /// independently attest the read-only staged mount and writable scratch
    /// inodes, then exclude writers through launch and settlement.
    pub fn bind_execution_inputs(
        mut self,
        retained: &ExternalGuestInputProjection,
        private_scratch: Vec<(usize, lillux::InheritedDescriptorAuthority)>,
    ) -> Result<BoundGuestMountedContent> {
        ensure!(
            retained.identity_digest()? == self.guest_input_identity
                && self.runtime_mounts.len() == retained.inputs.len()
                && self.content_records.len() == retained.record_descriptors().count(),
            "mounted guest content differs from retained projection"
        );
        let mut scratch = std::collections::BTreeMap::new();
        for (index, authority) in private_scratch {
            ensure!(
                scratch.insert(index, authority).is_none(),
                "private guest scratch slot was supplied twice"
            );
        }
        for (index, (slot, input)) in self
            .runtime_mounts
            .iter_mut()
            .zip(&retained.inputs)
            .enumerate()
        {
            if matches!(input.content_authority, GuestMountContentAuthority::PrivateScratch { .. }) {
                ensure!(slot.is_none(), "private guest scratch has a staged source");
                let authority = scratch
                    .remove(&index)
                    .context("private guest scratch slot is absent")?;
                let directory = authority.try_clone_pinned_directory(
                    std::path::PathBuf::from("<mounted-guest-scratch>"),
                )?;
                directory.require_owner_private_directory()?;
                ensure!(
                    directory.entries_no_follow_bounded(0)?.is_empty(),
                    "private guest scratch was populated before binding"
                );
                *slot = Some(authority);
            } else {
                ensure!(slot.is_some(), "immutable guest input has no mounted source");
            }
        }
        ensure!(scratch.is_empty(), "unexpected private guest scratch slot");
        let runtime_mounts = self
            .runtime_mounts
            .into_iter()
            .map(|slot| slot.context("guest runtime slot remained unbound"))
            .collect::<Result<Vec<_>>>()?;
        let runtime_descriptors = runtime_mounts
            .iter()
            .map(|handle| handle.inherited_descriptor().map_err(anyhow::Error::msg))
            .collect::<Result<Vec<_>>>()?;
        let record_descriptors = self
            .content_records
            .iter()
            .map(|handle| handle.inherited_descriptor().map_err(anyhow::Error::msg))
            .collect::<Result<Vec<_>>>()?;
        let workspace_descriptor = self
            .workspace_outputs
            .as_ref()
            .map(|handle| handle.inherited_descriptor().map_err(anyhow::Error::msg))
            .transpose()?;
        let mut execution_inputs = retained.clone();
        rebind_opened_guest_descriptors(
            &mut execution_inputs,
            workspace_descriptor,
            &runtime_descriptors,
            &record_descriptors,
        )?;
        Ok(BoundGuestMountedContent {
            execution_inputs,
            workspace_outputs: self.workspace_outputs,
            runtime_mounts,
            content_records: self.content_records,
        })
    }
}

/// Verify the exact opened authorities, not merely their stage pathnames.
/// This follows the full staged-content check and retains the descriptors
/// needed for the supervisor's source-mount preparation.
pub(crate) fn open_verified_staged_guest_content(
    staged: &StagedGuestPackage,
    retained: &ExternalGuestInputProjection,
) -> Result<VerifiedGuestContentHandles> {
    recheck_staged_guest_content(staged, retained)?;
    open_verified_mounted_guest_content(staged.root(), retained)
}

/// Open exact staged inputs from an already pinned guest source mount.
///
/// The owner must independently prove the mounted stage identity, read-only
/// access and continuous exclusion of other writers. This checks the opened
/// content against the retained projection and holds those same descriptors;
/// it does not grant supervisor or candidate launch authority.
pub fn open_verified_mounted_guest_content(
    root: &lillux::PinnedDirectory,
    retained: &ExternalGuestInputProjection,
) -> Result<VerifiedGuestContentHandles> {
    recheck_mounted_guest_content(root, retained)?;
    let workspace_outputs = retained
        .workspace_outputs
        .as_ref()
        .map(|output| {
            let (record, bytes) = open_staged_record_handle(
                root,
                "workspace_outputs",
                &output.authority_hash,
                output.bytes,
            )?;
            validate_workspace_output_bytes(&bytes, retained)?;
            Ok::<_, anyhow::Error>(record)
        })
        .transpose()?;
    let mut runtime_mounts = Vec::with_capacity(retained.inputs.len());
    let mut content_records = Vec::with_capacity(retained.record_descriptors().count());
    let mut record_index = 0usize;
    for (index, input) in retained.inputs.iter().enumerate() {
        if matches!(input.content_authority, GuestMountContentAuthority::PrivateScratch { .. }) {
            runtime_mounts.push(None);
            continue;
        }
        let name = format!("input-{index:02}");
        let source = match input.kind {
            GuestMountKind::Directory => root.open_inherited_mount_entry(OsStr::new(&name))?,
            GuestMountKind::RegularFile => root.open_inherited_regular(OsStr::new(&name), false)?,
        }
        .context("staged guest mount disappeared before descriptor custody")?;
        match &input.content_authority {
            GuestMountContentAuthority::ProductManifest {
                manifest_kind,
                manifest_hash,
                manifest_bytes,
                ..
            } => {
                let (manifest, _) = open_staged_record_handle(
                    root,
                    &format!("record-{record_index:02}"),
                    manifest_hash,
                    *manifest_bytes,
                )?;
                ryeos_state::external_content::realization_verification::verify_staged_external_realization(
                    &source,
                    &manifest,
                    match manifest_kind {
                        GuestProductManifestKind::Content =>
                            ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
                        GuestProductManifestKind::LargeContent =>
                            ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
                    },
                    manifest_hash,
                    *manifest_bytes,
                    match input.kind {
                        GuestMountKind::Directory => ryeos_state::objects::ExternalContentKind::Tree,
                        GuestMountKind::RegularFile => ryeos_state::objects::ExternalContentKind::File,
                    },
                    input.bytes,
                )?;
                content_records.push(manifest);
                record_index += 1;
            }
            GuestMountContentAuthority::SourceClosure {
                binding_hash,
                binding_bytes,
                manifest_hash,
                manifest_bytes,
                ..
            } => {
                let (binding, binding_bytes_value) = open_staged_record_handle(
                    root,
                    &format!("record-{record_index:02}"),
                    binding_hash,
                    *binding_bytes,
                )?;
                record_index += 1;
                let (manifest, manifest_bytes_value) = open_staged_record_handle(
                    root,
                    &format!("record-{record_index:02}"),
                    manifest_hash,
                    *manifest_bytes,
                )?;
                record_index += 1;
                let verified = ryeos_state::source_verification::VerifiedAdmittedSourceRecords::from_canonical_bytes(
                    binding_hash,
                    manifest_hash,
                    &binding_bytes_value,
                    &manifest_bytes_value,
                )?;
                ensure!(
                    verified.manifest().totals.total_bytes == input.bytes
                        && input.authority_id == verified.binding_hash()
                        && std::path::Path::new(&input.destination)
                            == verified.runtime_destination(),
                    "opened source records differ from retained projection"
                );
                let source_directory = source.try_clone_pinned_directory(
                    std::path::PathBuf::from("<staged-guest-source>"),
                )?;
                verified.verify_tree(&source_directory)?;
                content_records.push(binding);
                content_records.push(manifest);
            }
            GuestMountContentAuthority::RawFile { sha256 } => {
                let observed = source.regular_file_observation()?;
                ensure!(
                    observed.size() == input.bytes
                        && source.digest_regular_file_stable_exact(&observed)? == *sha256
                        && Some(observed.portable_mode()?) == input.normalized_mode,
                    "opened raw guest input changed its bytes or mode"
                );
            }
            GuestMountContentAuthority::PrivateScratch { .. } => unreachable!(),
        }
        runtime_mounts.push(Some(source));
    }
    ensure!(
        record_index == retained.record_descriptors().count(),
        "opened guest content record order changed"
    );
    Ok(VerifiedGuestContentHandles {
        guest_input_identity: retained.identity_digest()?,
        workspace_outputs,
        runtime_mounts,
        content_records,
    })
}

fn open_staged_record_handle(
    root: &lillux::PinnedDirectory,
    name: &str,
    expected_hash: &str,
    expected_bytes: u64,
) -> Result<(lillux::InheritedDescriptorAuthority, Vec<u8>)> {
    let record = root
        .open_inherited_regular(OsStr::new(name), false)?
        .context("staged guest record disappeared before descriptor custody")?;
    let (bytes, observation) = record.read_regular_file_stable_bounded(expected_bytes)?;
    ensure!(
        observation.size() == expected_bytes
            && bytes.len() as u64 == expected_bytes
            && lillux::sha256_hex(&bytes) == expected_hash,
        "opened guest record digest or size changed"
    );
    Ok((record, bytes))
}

/// Reuse state-owned exact product and source verification over the imported
/// descriptors. This never reconstructs authority from a live project path.
pub fn recheck_staged_guest_content(
    staged: &StagedGuestPackage,
    retained: &ExternalGuestInputProjection,
) -> Result<()> {
    retained.validate()?;
    ensure!(
        retained.identity_digest()? == staged.manifest().guest_input_identity,
        "staged guest content contradicts retained input identity"
    );
    recheck_mounted_guest_content(staged.root(), retained)
}

/// Verify the exact staged content from a pinned read-only guest mount.
///
/// This deliberately does not establish the mounted stage's identity or the
/// mount's read-only status. The outer source owner must prove both before the
/// trusted supervisor opens this root; the signed projection supplies the
/// expected content identities. It grants no candidate launch permission.
pub fn recheck_mounted_guest_content(
    root: &lillux::PinnedDirectory,
    retained: &ExternalGuestInputProjection,
) -> Result<()> {
    retained.validate()?;
    if let Some(output) = &retained.workspace_outputs {
        let bytes = read_staged_record(
            root,
            "workspace_outputs",
            &output.authority_hash,
            output.bytes,
        )?;
        validate_workspace_output_bytes(&bytes, retained)?;
    }

    let mut record_index = 0usize;
    for (index, input) in retained.inputs.iter().enumerate() {
        let name = format!("input-{index:02}");
        match &input.content_authority {
            GuestMountContentAuthority::ProductManifest {
                manifest_kind,
                manifest_hash,
                manifest_bytes,
                ..
            } => {
                let source = root
                    .open_inherited_mount_entry(OsStr::new(&name))?
                    .context("staged guest product disappeared")?;
                let manifest_name = format!("record-{record_index:02}");
                let manifest = root
                    .open_inherited_regular(OsStr::new(&manifest_name), false)?
                    .context("staged guest product manifest disappeared")?;
                let kind = match manifest_kind {
                    GuestProductManifestKind::Content => {
                        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND
                    }
                    GuestProductManifestKind::LargeContent => {
                        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
                    }
                };
                let content_kind = match input.kind {
                    GuestMountKind::Directory => ryeos_state::objects::ExternalContentKind::Tree,
                    GuestMountKind::RegularFile => ryeos_state::objects::ExternalContentKind::File,
                };
                ryeos_state::external_content::realization_verification::verify_staged_external_realization(
                    &source,
                    &manifest,
                    kind,
                    manifest_hash,
                    *manifest_bytes,
                    content_kind,
                    input.bytes,
                )?;
                record_index += 1;
            }
            GuestMountContentAuthority::SourceClosure {
                binding_hash,
                binding_bytes,
                manifest_hash,
                manifest_bytes,
                ..
            } => {
                let binding = read_staged_record(
                    root,
                    &format!("record-{record_index:02}"),
                    binding_hash,
                    *binding_bytes,
                )?;
                record_index += 1;
                let manifest = read_staged_record(
                    root,
                    &format!("record-{record_index:02}"),
                    manifest_hash,
                    *manifest_bytes,
                )?;
                record_index += 1;
                let verified = ryeos_state::source_verification::VerifiedAdmittedSourceRecords::from_canonical_bytes(
                    binding_hash,
                    manifest_hash,
                    &binding,
                    &manifest,
                )?;
                ensure!(
                    verified.manifest().totals.total_bytes == input.bytes
                        && input.authority_id == verified.binding_hash()
                        && std::path::Path::new(&input.destination)
                            == verified.runtime_destination(),
                    "staged guest source projection differs from its retained binding"
                );
                let source = root
                    .open_child_directory(OsStr::new(&name))?
                    .context("staged guest source disappeared")?;
                verified.verify_tree(&source)?;
            }
            GuestMountContentAuthority::RawFile { sha256 } => {
                let source = root
                    .open_pinned_regular(OsStr::new(&name), false)?
                    .context("staged guest configuration disappeared")?;
                let observation = source.observation()?;
                ensure!(
                    observation.size() == input.bytes
                        && source.digest_stable_exact(&observation)? == *sha256
                        && input.normalized_mode == Some(source.permission_mode()?),
                    "staged guest configuration bytes or mode changed"
                );
            }
            GuestMountContentAuthority::PrivateScratch { .. } => {}
        }
    }
    ensure!(
        record_index == retained.record_descriptors().count(),
        "staged guest content record order changed"
    );
    Ok(())
}

fn validate_workspace_output_bytes(
    bytes: &[u8],
    retained: &ExternalGuestInputProjection,
) -> Result<()> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    ensure!(
        lillux::canonical_json(&value)?.as_bytes() == bytes,
        "staged workspace-output authority is noncanonical"
    );
    let authority: ryeos_state::objects::WorkspaceOutputAuthority = serde_json::from_value(value)?;
    authority.validate()?;
    ensure!(
        authority.partition.roots.iter().all(|root| root.storage
            == ryeos_state::external_content::products::ProductStorage::Content),
        "staged workspace outputs require the transferable content tier"
    );
    let shadows: Vec<_> = retained
        .inputs
        .iter()
        .filter_map(|input| input.destination.strip_prefix("/workspace/"))
        .collect();
    for output_root in &authority.partition.roots {
        let output_path = std::path::Path::new(&output_root.path);
        ensure!(
            shadows.iter().all(|shadow| {
                let shadow = std::path::Path::new(shadow);
                !output_path.starts_with(shadow) && !shadow.starts_with(output_path)
            }),
            "staged workspace output overlaps an admitted input shadow"
        );
    }
    Ok(())
}

fn read_staged_record(
    root: &lillux::PinnedDirectory,
    name: &str,
    expected_hash: &str,
    expected_bytes: u64,
) -> Result<Vec<u8>> {
    let record = root
        .open_pinned_regular(OsStr::new(name), false)?
        .context("staged guest record disappeared")?;
    let observation = record.observation()?;
    ensure!(
        observation.size() == expected_bytes,
        "staged guest record length changed"
    );
    let bytes = record.read_stable_bounded(&observation, expected_bytes)?;
    ensure!(
        bytes.len() as u64 == expected_bytes && lillux::sha256_hex(&bytes) == expected_hash,
        "staged guest record digest changed"
    );
    Ok(bytes)
}
