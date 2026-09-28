//! Descriptor-relative capture of an already-admitted workspace-output partition.
//!
//! This module does not fence writers or choose lifecycle ownership. Its caller
//! must hold the exact native writer-exclusion proof and one guarded publication
//! stage spanning the source snapshot and output capture.

use std::collections::BTreeMap;
use std::ffi::OsStr;

use anyhow::bail;
use ryeos_state::external_content::products::ProductStorage;
use ryeos_state::ignore::{IgnoreConfig, IgnoreMatcher};
use ryeos_state::objects::{
    ProjectSnapshotPolicy, WorkspaceOutputCaptureState, WorkspaceOutputPartition,
};

pub trait WorkspaceOutputObjectStage {
    fn store_workspace_output_object(
        &mut self,
        guard: &ryeos_state::CasMutationGuard,
        cas: &lillux::CasStore,
        value: &serde_json::Value,
    ) -> anyhow::Result<String>;
}

impl WorkspaceOutputObjectStage for ryeos_state::StagedCasRootLease {
    fn store_workspace_output_object(
        &mut self,
        guard: &ryeos_state::CasMutationGuard,
        cas: &lillux::CasStore,
        value: &serde_json::Value,
    ) -> anyhow::Result<String> {
        self.store_object_admitted(guard, cas, value)
    }
}

impl WorkspaceOutputObjectStage for ryeos_state::DurableCasUploadStage {
    fn store_workspace_output_object(
        &mut self,
        guard: &ryeos_state::CasMutationGuard,
        cas: &lillux::CasStore,
        value: &serde_json::Value,
    ) -> anyhow::Result<String> {
        self.store_object(guard, cas, value)
    }
}

pub fn capture_native_workspace_outputs(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    stage: &mut impl WorkspaceOutputObjectStage,
    project: &lillux::PinnedDirectory,
    partition: &WorkspaceOutputPartition,
    policy: &ProjectSnapshotPolicy,
) -> anyhow::Result<BTreeMap<String, WorkspaceOutputCaptureState>> {
    authority.ensure_guard(guard)?;
    let matcher = validate_capture_policy(partition, policy)?;
    project.ensure_path_binding()?;
    let cas = authority.cas_store()?;
    let mut outputs = BTreeMap::new();
    for root in &partition.roots {
        let content_policy = ryeos_state::ExternalCapturePolicy::new(root.path.clone(), &matcher)?;
        let Some(directory) = open_output_directory(project, &root.path)? else {
            outputs.insert(root.name.clone(), WorkspaceOutputCaptureState::Absent);
            continue;
        };
        let mut sink = OutputSink {
            cas: &cas,
            authority,
            guard,
        };
        let state = match root.storage {
            ProductStorage::Content => {
                let bounds = &root.effective_bounds;
                let mut budget = ryeos_state::LaunchCaptureBudget::bounded(
                    bounds.maximum_depth,
                    bounds.maximum_entries,
                    bounds.maximum_file_bytes,
                    bounds.maximum_total_bytes,
                )?;
                let manifest = ryeos_state::external_content::capture_tree(
                    &directory,
                    &[],
                    &content_policy,
                    &mut budget,
                    &mut sink,
                )?;
                if manifest.entries.is_empty() {
                    WorkspaceOutputCaptureState::EmptyDirectory
                } else {
                    let hash = stage.store_workspace_output_object(
                        guard,
                        &cas,
                        &serde_json::to_value(&manifest)?,
                    )?;
                    WorkspaceOutputCaptureState::Captured {
                        manifest_kind: manifest.kind,
                        manifest_hash: hash,
                    }
                }
            }
            ProductStorage::LargeContent => {
                let bounds = &root.effective_bounds;
                let capture_policy = ryeos_state::LargeContentCapturePolicy::new(
                    root.path.clone(),
                    &matcher,
                    ryeos_state::LargeContentCaptureBounds {
                        max_depth: bounds.maximum_depth,
                        max_entries: bounds.maximum_entries,
                        max_file_bytes: bounds.maximum_file_bytes,
                        max_total_bytes: bounds.maximum_total_bytes,
                    },
                )?;
                match ryeos_state::external_content::capture_large_tree_optional(
                    &directory,
                    &capture_policy,
                    &mut sink,
                )? {
                    None => WorkspaceOutputCaptureState::EmptyDirectory,
                    Some(manifest) => {
                        let hash = stage.store_workspace_output_object(
                            guard,
                            &cas,
                            &manifest.to_value()?,
                        )?;
                        WorkspaceOutputCaptureState::Captured {
                            manifest_kind: manifest.kind,
                            manifest_hash: hash,
                        }
                    }
                }
            }
        };
        directory.ensure_path_binding()?;
        project.ensure_path_binding()?;
        outputs.insert(root.name.clone(), state);
    }
    project.ensure_path_binding()?;
    Ok(outputs)
}

fn validate_capture_policy(
    partition: &WorkspaceOutputPartition,
    policy: &ProjectSnapshotPolicy,
) -> anyhow::Result<IgnoreMatcher> {
    partition.validate()?;
    policy.validate()?;
    if partition.project_snapshot_policy_hash
        != ryeos_state::objects::canonical_value_digest(&policy.to_value())?
        || partition.capture_policy_digest != partition.derived_capture_policy_digest(policy)?
    {
        bail!("workspace output capture must use the exact admitted snapshot policy");
    }
    IgnoreMatcher::from_config(&IgnoreConfig {
        patterns: policy.node_patterns.clone(),
    })
}

fn open_output_directory(
    project: &lillux::PinnedDirectory,
    relative: &str,
) -> anyhow::Result<Option<lillux::PinnedDirectory>> {
    let mut directory = project.try_clone()?;
    let (device, _) = project.device_inode()?;
    for component in relative.split('/') {
        let Some(child) = directory.open_child_directory(OsStr::new(component))? else {
            return Ok(None);
        };
        if child.device_inode()?.0 != device {
            bail!("workspace output crossed the admitted project filesystem");
        }
        directory = child;
    }
    Ok(Some(directory))
}

struct OutputSink<'a> {
    cas: &'a lillux::CasStore,
    authority: &'a ryeos_state::PinnedStateAuthority,
    guard: &'a ryeos_state::CasMutationGuard,
}

impl ryeos_state::ExternalContentBlobSink for OutputSink<'_> {
    fn store_file(
        &mut self,
        file: std::fs::File,
        path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)> {
        self.authority.ensure_guard(self.guard)?;
        let captured = self.cas.put_blob_from_open_regular_bounded(
            file,
            std::path::Path::new(path),
            ryeos_state::MAX_CAPTURE_FILE_BYTES,
        )?;
        if captured.size != expected_size {
            bail!("workspace output changed size during capture");
        }
        Ok((captured.hash, captured.size))
    }
}

impl ryeos_state::ExternalLargeContentSink for OutputSink<'_> {
    fn store_large_file(
        &mut self,
        file: std::fs::File,
        identity: ryeos_state::PinnedLargeObjectSourceIdentity,
        relative_path: &str,
        expected_sha256: Option<&str>,
    ) -> anyhow::Result<ryeos_state::IngestedLargeObject> {
        self.authority.ensure_guard(self.guard)?;
        self.authority.large_object_store()?.ingest_open_regular(
            file,
            identity,
            relative_path,
            expected_sha256,
        )
    }

    fn store_content_file(
        &mut self,
        file: std::fs::File,
        path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)> {
        ryeos_state::ExternalContentBlobSink::store_file(self, file, path, expected_size)
    }
}
