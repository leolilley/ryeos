//! Redemption checks for already-admitted source records and their exact tree.
//!
//! Expected hashes must come from retained launch authority. This module does
//! not resolve source, admit a publisher, acquire writer exclusion, or attest a
//! new source product. Host observations remain descriptor-relative Lillux
//! operations; their interpretation uses the existing source CAS schemas.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::objects::{
    EffectiveSourceBinding, EffectiveSourceClosureProjection, MAX_SOURCE_BINDING_BYTES,
    MAX_SOURCE_MANIFEST_BYTES, SourceClosureManifest, SourceFileMode, SourceLogicalBinding,
};

/// Immutable, bounded records checked against an independently admitted pair.
/// The type intentionally does not deserialize: unchecked bytes cannot acquire
/// verified-record status through a wire decoder.
#[derive(Debug, Clone)]
pub struct VerifiedAdmittedSourceRecords {
    binding: EffectiveSourceBinding,
    manifest: SourceClosureManifest,
    binding_hash: String,
    content_manifest_hash: String,
    owner_key: String,
    sealed_identity_env: String,
    logical_project_mount: String,
    runtime_relative_mount: String,
}

impl VerifiedAdmittedSourceRecords {
    pub fn from_canonical_bytes(
        expected_binding_hash: &str,
        expected_manifest_hash: &str,
        binding_bytes: &[u8],
        manifest_bytes: &[u8],
    ) -> anyhow::Result<Self> {
        let binding_value = canonical_record(
            "admitted source binding",
            expected_binding_hash,
            binding_bytes,
            MAX_SOURCE_BINDING_BYTES,
        )?;
        let manifest_value = canonical_record(
            "admitted source manifest",
            expected_manifest_hash,
            manifest_bytes,
            MAX_SOURCE_MANIFEST_BYTES,
        )?;
        let binding = EffectiveSourceBinding::from_value(&binding_value)?;
        let manifest = SourceClosureManifest::from_value(&manifest_value)?;
        if binding.digest()? != expected_binding_hash
            || manifest.digest()? != expected_manifest_hash
        {
            anyhow::bail!("admitted source typed records contradict their expected identities");
        }
        binding.validate_content_manifest(&manifest)?;
        // The materialized source namespace represents the existing single
        // `source` root, not an implicit flattening of unrelated logical roots.
        validate_materialized_manifest(&manifest)?;
        let owner_key = binding.owner_key()?;
        let sealed_identity_env = lillux::canonical_json(&serde_json::json!({
            "schema": binding.schema,
            "binding_hash": expected_binding_hash,
            "content_manifest_hash": expected_manifest_hash,
            "owner_key": owner_key,
        }))?;
        if sealed_identity_env.len() > 2048 {
            anyhow::bail!("admitted source identity exceeds its protected environment bound");
        }
        let logical_project_mount = logical_project_mount(&binding)?;
        Ok(Self {
            binding,
            manifest,
            binding_hash: expected_binding_hash.to_owned(),
            content_manifest_hash: expected_manifest_hash.to_owned(),
            owner_key,
            sealed_identity_env,
            logical_project_mount,
            runtime_relative_mount: format!("source-closures/{expected_binding_hash}"),
        })
    }

    pub fn binding(&self) -> &EffectiveSourceBinding {
        &self.binding
    }
    pub fn manifest(&self) -> &SourceClosureManifest {
        &self.manifest
    }
    pub fn binding_hash(&self) -> &str {
        &self.binding_hash
    }
    pub fn content_manifest_hash(&self) -> &str {
        &self.content_manifest_hash
    }
    pub fn owner_key(&self) -> &str {
        &self.owner_key
    }
    pub fn sealed_identity_env(&self) -> &str {
        &self.sealed_identity_env
    }
    pub fn logical_entry(&self) -> &str {
        logical_entry(&self.binding)
    }
    pub fn logical_project_mount(&self) -> &str {
        &self.logical_project_mount
    }
    pub fn runtime_relative_mount(&self) -> &str {
        &self.runtime_relative_mount
    }

    /// Workload namespace coordinate; never authority to open controller files.
    pub fn runtime_destination(&self) -> PathBuf {
        Path::new(crate::objects::EXECUTION_RUNTIME_REALIZATIONS_ROOT)
            .join(&self.runtime_relative_mount)
    }

    pub fn runtime_entry_path(&self) -> PathBuf {
        self.runtime_destination().join(self.logical_entry())
    }

    pub fn validate_projection(
        &self,
        projection: &EffectiveSourceClosureProjection,
    ) -> anyhow::Result<()> {
        projection.validate()?;
        if projection.schema != self.binding.schema
            || projection.binding_hash != self.binding_hash
            || projection.content_manifest_hash != self.content_manifest_hash
            || projection.owner_key != self.owner_key
            || projection.file_count != self.manifest.totals.file_count
            || projection.total_bytes != self.manifest.totals.total_bytes
        {
            anyhow::bail!("admitted source records contradict their effective projection");
        }
        Ok(())
    }

    pub fn verify_tree(&self, root: &lillux::PinnedDirectory) -> anyhow::Result<()> {
        verify_admitted_source_tree(root, &self.manifest)
    }
}

fn canonical_record(
    label: &str,
    expected_hash: &str,
    bytes: &[u8],
    max_bytes: usize,
) -> anyhow::Result<serde_json::Value> {
    crate::objects::thread_snapshot::validate_canonical_hash(label, expected_hash)?;
    if bytes.is_empty() || bytes.len() > max_bytes {
        anyhow::bail!("{label} exceeds its bounded record size");
    }
    if lillux::sha256_hex(bytes) != expected_hash {
        anyhow::bail!("{label} bytes contradict their admitted digest");
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    if lillux::canonical_json(&value)?.as_bytes() != bytes {
        anyhow::bail!("{label} is not exact canonical JSON");
    }
    Ok(value)
}

fn validate_materialized_manifest(manifest: &SourceClosureManifest) -> anyhow::Result<()> {
    manifest.validate()?;
    if manifest.roots.len() != 1
        || manifest.roots[0].id != "source"
        || manifest.entries.iter().any(|entry| entry.root != "source")
    {
        anyhow::bail!("admitted source tree requires its exact single source root");
    }
    Ok(())
}

/// Check exact membership, bytes, lengths and portable regular-file modes.
/// Symlinks, special entries and even empty undeclared directories are refused.
/// This is an observation, not a writer-exclusion/freeze guarantee: callers
/// retain the independent lifetime and read-only authority used for execution.
pub fn verify_admitted_source_tree(
    root: &lillux::PinnedDirectory,
    manifest: &SourceClosureManifest,
) -> anyhow::Result<()> {
    validate_materialized_manifest(manifest)?;
    let expected = manifest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut directories = BTreeSet::new();
    for path in expected.keys() {
        let mut parent = Path::new(path).parent();
        while let Some(value) = parent.filter(|value| !value.as_os_str().is_empty()) {
            directories.insert(
                value
                    .to_str()
                    .expect("validated UTF-8 source path")
                    .to_owned(),
            );
            parent = value.parent();
        }
    }
    let mut observed = BTreeSet::new();
    let mut pending = vec![(root.try_clone()?, String::new())];
    // Iteration avoids host-stack growth for deep but bounded admitted paths.
    while let Some((directory, prefix)) = pending.pop() {
        for actual in directory.entries_no_follow_bounded(expected.len().saturating_add(1))? {
            let name = actual
                .name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("admitted source has a non-UTF-8 entry"))?;
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            match actual.entry_type {
                lillux::PinnedEntryType::Directory => {
                    if !directories.remove(&path) {
                        anyhow::bail!("admitted source has unexpected directory {path}");
                    }
                    let child = directory
                        .open_child_directory(&actual.name)?
                        .ok_or_else(|| {
                            anyhow::anyhow!("admitted source directory {path} disappeared")
                        })?;
                    pending.push((child, path));
                }
                lillux::PinnedEntryType::Regular => {
                    let entry = expected.get(path.as_str()).ok_or_else(|| {
                        anyhow::anyhow!("admitted source has unexpected file {path}")
                    })?;
                    let mut file =
                        directory
                            .open_regular(&actual.name, false)?
                            .ok_or_else(|| {
                                anyhow::anyhow!("admitted source file {path} disappeared")
                            })?;
                    let (digest, metadata) =
                        lillux::digest_open_regular_file_stable_exact(&mut file, entry.size)?;
                    let expected_mode = match entry.mode {
                        SourceFileMode::ReadOnly => 0o644,
                        SourceFileMode::Executable => 0o755,
                    };
                    if digest != entry.blob_hash
                        || lillux::normalized_portable_regular_mode(&metadata)? != expected_mode
                    {
                        anyhow::bail!("admitted source file {path} failed verification");
                    }
                    if !observed.insert(path) {
                        anyhow::bail!("admitted source contains a repeated file observation");
                    }
                }
                _ => anyhow::bail!("admitted source contains unsupported entry {path}"),
            }
        }
    }
    if !directories.is_empty()
        || observed.iter().map(String::as_str).collect::<Vec<_>>()
            != expected.keys().copied().collect::<Vec<_>>()
    {
        anyhow::bail!("admitted source has missing or extra files");
    }
    Ok(())
}

pub fn logical_project_mount(binding: &EffectiveSourceBinding) -> anyhow::Result<String> {
    let directory = binding
        .kind_ceiling
        .schema_document
        .get("location")
        .and_then(|location| location.get("directory"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("retained source kind has no logical directory"))?;
    crate::objects::validate_canonical_project_relative_path(directory)?;
    let mut path = PathBuf::from(".ai").join(directory);
    match &binding.logical_binding {
        SourceLogicalBinding::Tool { .. } => {
            if let Some((namespace, _)) = binding.owner.logical_item_key.split_once('/') {
                path.push(namespace);
            }
        }
        SourceLogicalBinding::ToolDirectory { root, .. } => path.push(root),
        SourceLogicalBinding::Worker { root, .. } => {
            let namespace = binding
                .owner
                .logical_item_key
                .split('/')
                .next()
                .ok_or_else(|| anyhow::anyhow!("worker source owner has no namespace"))?;
            path.push(namespace);
            path.push(root);
        }
    }
    let value = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("admitted source mount is not UTF-8"))?
        .to_owned();
    crate::objects::validate_canonical_project_relative_path(&value)?;
    Ok(value)
}

pub fn logical_entry(binding: &EffectiveSourceBinding) -> &str {
    match &binding.logical_binding {
        SourceLogicalBinding::Tool { root_entry, .. }
        | SourceLogicalBinding::ToolDirectory { root_entry, .. } => root_entry,
        SourceLogicalBinding::Worker { entry, .. } => entry,
    }
}

#[cfg(test)]
mod tests;
