//! Descriptor-bound product inputs shared by external session and direct execution.
//!
//! This redeems the existing realization inventory; it does not capture new
//! products, admit an endpoint, acquire provider authority or launch a guest.
//! The caller owns its exact runtime/source policy and the complete projection.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_external_execution_contract::{
    GuestMountAccess, GuestMountContentAuthority, GuestMountInput, GuestMountKind, GuestMountRole,
    GuestProductManifestKind, MAX_GUEST_INPUTS,
};
use ryeos_state::objects::{ExternalContentKind, ExternalContentRealization};

/// A destination supplied by the caller's admitted runtime recipe. Ordinary
/// direct execution passes no override and preserves each natural mount root.
pub(crate) struct RuntimeDestinationOverride<'a> {
    pub realization_id: &'a str,
    pub destination: &'a Path,
}

pub(crate) struct PreparedProductInputs {
    pub inputs: Vec<GuestMountInput>,
    pub authorities: Vec<lillux::InheritedDescriptorAuthority>,
    pub manifest_authorities: Vec<lillux::InheritedDescriptorAuthority>,
    /// Retain until consumers finish, or acknowledge an exact private copy.
    /// An open directory alone does not prevent cache retirement of children.
    pub leases: Vec<std::fs::File>,
    pub destinations: BTreeMap<String, String>,
}

pub(crate) fn prepare_product_inputs(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    project_root: &Path,
    runtime_override: Option<RuntimeDestinationOverride<'_>>,
) -> Result<PreparedProductInputs> {
    let bound = super::external_content::bind_external_guest_realizations(state, resolution)?
        .context("external guest has no exact product realization inventory")?;
    prepare_bound_product_inputs(state, bound, project_root, runtime_override)
}

/// Redeem an authenticated retained execution inventory without synthesizing
/// a live ResolutionOutput or a Worker bootstrap. The caller remains the
/// admitted execution owner; this helper grants no placement/contact authority.
pub(crate) fn prepare_retained_product_inputs(
    state: &ryeos_app::state::AppState,
    realizations: &ryeos_state::objects::ExternalContentRealizationSet,
    project_root: &Path,
    runtime_override: Option<RuntimeDestinationOverride<'_>>,
) -> Result<PreparedProductInputs> {
    let bound =
        super::external_content::bind_retained_external_guest_realizations(state, realizations)?
            .context("retained guest inputs have no exact realization inventory")?;
    prepare_bound_product_inputs(state, bound, project_root, runtime_override)
}

fn prepare_bound_product_inputs(
    state: &ryeos_app::state::AppState,
    bound: super::external_content::BoundExternalRealizations,
    project_root: &Path,
    runtime_override: Option<RuntimeDestinationOverride<'_>>,
) -> Result<PreparedProductInputs> {
    let (realized, sources, leases) = bound.into_external_guest_parts();
    let entries = realized.iter().cloned().collect::<Vec<_>>();
    ensure!(
        !leases.is_empty(),
        "external guest realization inventory has no retained materialization leases"
    );
    ensure!(
        !entries.is_empty() && entries.len() <= MAX_GUEST_INPUTS && entries.len() == sources.len(),
        "external guest realization authority count changed or exceeds its bound"
    );
    let mut destinations = BTreeMap::new();
    let mut inputs = Vec::with_capacity(entries.len());
    let mut authorities = Vec::with_capacity(entries.len());
    let mut manifest_authorities = Vec::with_capacity(entries.len());
    for (entry, source) in entries.iter().zip(sources) {
        let destination = product_destination(entry, project_root, runtime_override.as_ref())?;
        ensure!(
            destinations
                .insert(entry.id.clone(), destination.clone())
                .is_none(),
            "external guest realization identity is duplicated"
        );
        let descriptor = source.inherited_descriptor().map_err(anyhow::Error::msg)?;
        let normalized_mode = match entry.kind {
            ExternalContentKind::Tree => None,
            ExternalContentKind::File => Some(source.regular_file_observation()?.portable_mode()?),
        };
        let manifest_value = {
            let cas_read = state.acquire_cas_read()?;
            cas_read
                .cas()
                .get_object(&entry.manifest_hash)?
                .with_context(|| {
                    format!(
                        "external guest realization `{}` lost its retained manifest",
                        entry.id
                    )
                })?
        };
        let (manifest_kind, manifest_json) = validated_manifest(entry, &manifest_value)?;
        let manifest_descriptor =
            lillux::sealed_memfd(c"ryeos-guest-product-manifest", manifest_json.as_bytes())
                .map_err(anyhow::Error::msg)?;
        let manifest_fd = manifest_descriptor
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?;
        inputs.push(GuestMountInput {
            role: GuestMountRole::Product,
            authority_id: entry.id.clone(),
            descriptor,
            destination,
            kind: match entry.kind {
                ExternalContentKind::Tree => GuestMountKind::Directory,
                ExternalContentKind::File => GuestMountKind::RegularFile,
            },
            access: GuestMountAccess::ReadOnly,
            normalized_mode,
            content_authority: GuestMountContentAuthority::ProductManifest {
                manifest_kind,
                manifest_hash: entry.manifest_hash.clone(),
                manifest_descriptor: manifest_fd,
                manifest_bytes: manifest_json.len() as u64,
            },
            bytes: entry.total_bytes,
        });
        authorities.push(source);
        manifest_authorities.push(manifest_descriptor);
    }
    if let Some(runtime) = runtime_override {
        ensure!(
            destinations.contains_key(runtime.realization_id),
            "external guest runtime product has no realized authority"
        );
    }
    Ok(PreparedProductInputs {
        inputs,
        authorities,
        manifest_authorities,
        leases,
        destinations,
    })
}

fn product_destination(
    entry: &ExternalContentRealization,
    project_root: &Path,
    runtime_override: Option<&RuntimeDestinationOverride<'_>>,
) -> Result<String> {
    let destination = match runtime_override.filter(|runtime| entry.id == runtime.realization_id) {
        Some(runtime) => runtime.destination.to_path_buf(),
        None => entry
            .mount_root
            .destination(Some(project_root), &entry.mount)?,
    };
    ensure!(
        destination.is_absolute()
            && destination
                .components()
                .enumerate()
                .all(|(index, component)| {
                    matches!(
                        (index, component),
                        (0, Component::RootDir) | (_, Component::Normal(_))
                    )
                }),
        "external guest realization destination is not absolute and normalized"
    );
    let text = destination
        .to_str()
        .context("external guest realization destination is not UTF-8")?;
    ensure!(
        text.len() <= 4096 && !text.chars().any(char::is_control) && destination != Path::new("/"),
        "external guest realization destination is invalid"
    );
    Ok(text.to_owned())
}

fn validated_manifest(
    entry: &ExternalContentRealization,
    value: &serde_json::Value,
) -> Result<(GuestProductManifestKind, String)> {
    let kind = match value.get("kind").and_then(serde_json::Value::as_str) {
        Some(ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND) => {
            let manifest = ryeos_state::objects::ExternalContentManifestObject::from_value(value)?;
            ensure!(
                manifest.entry_count == entry.entry_count
                    && manifest.total_bytes == entry.total_bytes
                    && (entry.kind != ExternalContentKind::File || manifest.is_file_shaped()),
                "external guest content manifest contradicts its realized input"
            );
            GuestProductManifestKind::Content
        }
        Some(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND) => {
            let manifest =
                ryeos_state::objects::ExternalLargeContentManifestObject::from_value(value)?;
            ensure!(
                manifest.entry_count == entry.entry_count
                    && manifest.total_bytes == entry.total_bytes
                    && (entry.kind != ExternalContentKind::File || manifest.is_file_shaped()),
                "external guest large-content manifest contradicts its realized input"
            );
            GuestProductManifestKind::LargeContent
        }
        _ => bail!("external guest realization has an unsupported manifest kind"),
    };
    let canonical = lillux::canonical_json(value)?;
    // This is the existing product-record wire bound, before descriptor creation.
    ensure!(
        !canonical.is_empty() && canonical.len() <= 8 * 1024 * 1024,
        "external guest product manifest exceeds its transfer bound"
    );
    ensure!(
        lillux::sha256_hex(canonical.as_bytes()) == entry.manifest_hash,
        "external guest realization manifest bytes changed their CAS identity"
    );
    Ok((kind, canonical))
}

#[cfg(test)]
mod tests;
