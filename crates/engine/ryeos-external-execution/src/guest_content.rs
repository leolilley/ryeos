//! Recheck imported guest content against the retained activation projection.
//!
//! This is an observation over a private staged generation, not a Ready or
//! writer-exclusion claim. The caller must pass the independently retained
//! projection, own the stage exclusively, and repeat the checks at the fixed-FD
//! supervisor adoption boundary before authorizing execution.

use std::ffi::OsStr;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::{
    ExternalGuestInputProjection, GuestMountContentAuthority, GuestMountKind,
    GuestProductManifestKind,
};

use crate::guest_staging::StagedGuestPackage;

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
    let root = staged.root();
    if let Some(output) = &retained.workspace_outputs {
        let bytes = read_staged_record(
            root,
            "workspace_outputs",
            &output.authority_hash,
            output.bytes,
        )?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(
            lillux::canonical_json(&value)?.as_bytes() == bytes,
            "staged workspace-output authority is noncanonical"
        );
        let authority: ryeos_state::objects::WorkspaceOutputAuthority =
            serde_json::from_value(value)?;
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
