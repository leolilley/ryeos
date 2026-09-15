//! Capture and restoration of an admitted workspace output partition.
//!
//! These primitives do not fence processes, choose output ownership, advance a
//! workspace journal, or publish a terminal result. Their lifecycle caller must
//! hold the existing workspace fence and publish source/output identities as one
//! generation. Native traversal, normalized adapter deltas and cold restoration
//! invoke these under the
//! existing lifecycle fence; these helpers cannot advance a generation alone.

pub(crate) mod admission;
mod delta;
pub(crate) use delta::apply_output_delta;

use std::collections::BTreeMap;
use std::ffi::OsStr;

use anyhow::bail;
use ryeos_state::external_content::products::{ProductBounds, ProductStorage};
use ryeos_state::ignore::{IgnoreConfig, IgnoreMatcher};
use ryeos_state::objects::workspace_output_capture::{
    WorkspaceOutputCapture, WorkspaceOutputCaptureState, WorkspaceOutputPartition,
    WorkspaceOutputRoot,
};
use ryeos_state::objects::{
    ExternalContentManifestObject, ExternalLargeContentManifestObject, ProjectSnapshotPolicy,
};

use super::external_content::{
    PrivateMaterializationBudget, ensure_materialization_parent, restore_workspace_output_tree,
};

/// Capture only the explicitly admitted roots in an already-fenced native
/// workspace. Intermediate absence/emptiness is not final-product success.
/// Every new manifest extends the caller's existing source-result stage while
/// the shared guard protects payload ingest; no independent publication exists.
#[allow(clippy::too_many_arguments)]
pub(crate) fn capture_native_workspace_outputs(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    stage: &mut ryeos_state::StagedCasRootLease,
    project: &lillux::PinnedDirectory,
    partition: &WorkspaceOutputPartition,
    policy: &ProjectSnapshotPolicy,
    isolation: &ryeos_engine::isolation::IsolationRuntime,
) -> anyhow::Result<BTreeMap<String, WorkspaceOutputCaptureState>> {
    require_native_backend(isolation)?;
    authority.ensure_guard(guard)?;
    let matcher = validate_capture_policy(partition, policy)?;
    project.ensure_path_binding()?;
    let cas = authority.cas_store()?;
    let mut outputs = BTreeMap::new();
    for root in &partition.roots {
        // Validate ancestors before testing optional absence: an excluded
        // ancestor is a refusal, not an absent output.
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
                    let hash = stage.store_object_admitted(
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
                        let hash =
                            stage.store_object_admitted(guard, &cas, &manifest.to_value()?)?;
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

/// Restore outputs only after the existing full source proof succeeds. This
/// never changes the source verifier's definition of an exact project tree.
/// A partial failure leaves an unready workspace for its lifecycle owner to
/// discard/recover; it cannot be admitted as a source-only successful restore.
#[allow(clippy::too_many_arguments)]
pub(crate) fn restore_workspace_outputs_before_view(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    source: &ryeos_state::project_materialization::PinnedProjectMaterialization,
    capture: &WorkspaceOutputCapture,
    policy: &ProjectSnapshotPolicy,
    budget: &PrivateMaterializationBudget,
) -> anyhow::Result<()> {
    authority.ensure_guard(guard)?;
    capture.validate()?;
    let matcher = validate_capture_policy(&capture.partition, policy)?;
    if source.snapshot_hash() != capture.result_project_snapshot_hash {
        bail!("workspace output capture does not belong to the proven source generation");
    }
    source.ensure_path_binding()?;
    let project = source.try_clone_root()?;
    source.ensure_root_binding()?;
    let cas = authority.cas_store()?;
    let result_snapshot = ryeos_state::project_materialization::load_project_snapshot_bounded(
        &cas,
        source.snapshot_hash(),
    )?
    .ok_or_else(|| anyhow::anyhow!("workspace output result snapshot is unavailable"))?;
    if result_snapshot.effective_policy_hash != capture.partition.project_snapshot_policy_hash {
        bail!("workspace output partition policy contradicts its source snapshot");
    }
    // Validate the entire manifest/policy set and absence of targets before
    // making any output filesystem changes.
    for root in &capture.partition.roots {
        ryeos_state::ExternalCapturePolicy::new(root.path.clone(), &matcher)?;
        if open_output_directory(&project, &root.path)?.is_some() {
            bail!("workspace output target already exists before restoration");
        }
        if let WorkspaceOutputCaptureState::Captured { manifest_hash, .. } =
            &capture.outputs[&root.name]
        {
            validate_retained_manifest(&cas, root, manifest_hash, &matcher)?;
        }
    }
    for root in &capture.partition.roots {
        let state = &capture.outputs[&root.name];
        if matches!(state, WorkspaceOutputCaptureState::Absent) {
            continue;
        }
        let (parent, name) = ensure_materialization_parent(&project, &root.path)?;
        let target = parent.create_child(&name, 0o755)?;
        if let WorkspaceOutputCaptureState::Captured { manifest_hash, .. } = state {
            restore_workspace_output_tree(
                authority,
                guard,
                &target,
                manifest_hash,
                root.storage,
                budget,
            )?;
        }
        target.ensure_path_binding()?;
    }
    // The full source verifier would now (correctly) see extra output files.
    // The source root inode remains the authority; each restored output was
    // separately verified by the existing manifest materializer above.
    project.ensure_path_binding()?;
    source.ensure_root_binding()?;
    Ok(())
}

fn require_native_backend(
    isolation: &ryeos_engine::isolation::IsolationRuntime,
) -> anyhow::Result<()> {
    if isolation.is_enforced() {
        bail!(
            "native output traversal cannot capture an enforced workspace; use its pinned frozen mutation evidence"
        );
    }
    Ok(())
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
    // Project exclusions belong to source capture. Outputs retain the exact
    // shared floor and admitted node patterns, not a new ignore language.
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

fn validate_retained_manifest(
    cas: &lillux::CasStore,
    root: &WorkspaceOutputRoot,
    hash: &str,
    matcher: &IgnoreMatcher,
) -> anyhow::Result<()> {
    match root.storage {
        ProductStorage::Content => {
            let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
                cas,
                hash,
                ryeos_state::objects::MAX_EXTERNAL_CONTENT_MANIFEST_BYTES as u64,
            )?;
            let manifest = ExternalContentManifestObject::from_value(&value)?;
            validate_manifest_entries(
                root,
                manifest.entry_count,
                manifest.total_bytes,
                manifest
                    .entries
                    .iter()
                    .map(|entry| (entry.path.as_str(), entry.kind, entry.size)),
                matcher,
            )
        }
        ProductStorage::LargeContent => {
            let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
                cas,
                hash,
                ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64,
            )?;
            let manifest = ExternalLargeContentManifestObject::from_value(&value)?;
            validate_manifest_entries(
                root,
                manifest.entry_count,
                manifest.total_bytes,
                manifest
                    .entries
                    .iter()
                    .map(|entry| (entry.path.as_str(), entry.kind, entry.size)),
                matcher,
            )
        }
    }
}

fn validate_manifest_entries<'a>(
    root: &WorkspaceOutputRoot,
    count: usize,
    total_bytes: u64,
    entries: impl IntoIterator<
        Item = (
            &'a str,
            ryeos_state::objects::ExternalContentManifestEntryKind,
            Option<u64>,
        ),
    >,
    matcher: &IgnoreMatcher,
) -> anyhow::Result<()> {
    let ProductBounds {
        maximum_entries,
        maximum_depth,
        maximum_file_bytes,
        maximum_total_bytes,
    } = root.effective_bounds;
    if count == 0 || count > maximum_entries || total_bytes > maximum_total_bytes {
        bail!("workspace output manifest contradicts admitted root bounds");
    }
    for (path, kind, size) in entries {
        let depth = path.split('/').count();
        if depth > maximum_depth
            || (kind == ryeos_state::objects::ExternalContentManifestEntryKind::Dir
                && depth >= maximum_depth)
            || size.is_some_and(|size| size > maximum_file_bytes)
        {
            bail!("workspace output member contradicts admitted root bounds");
        }
        ryeos_state::ExternalCapturePolicy::new(format!("{}/{}", root.path, path), matcher)?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::objects::workspace_output_capture::{
        WORKSPACE_OUTPUT_CAPTURE_KIND, WORKSPACE_OUTPUT_CAPTURE_SCHEMA,
        WORKSPACE_OUTPUT_PARTITION_SCHEMA,
    };
    use std::io::Write as _;

    fn partition(
        storage: ProductStorage,
        policy: &ProjectSnapshotPolicy,
    ) -> WorkspaceOutputPartition {
        let bounds = ProductBounds {
            maximum_entries: 32,
            maximum_depth: 8,
            maximum_file_bytes: 1024,
            maximum_total_bytes: 4096,
        };
        let mut partition = WorkspaceOutputPartition {
            schema: WORKSPACE_OUTPUT_PARTITION_SCHEMA.to_owned(),
            recipe_binding: "product_recipe".to_owned(),
            recipe_ref: "config:test/outputs".to_owned(),
            recipe_raw_content_digest: "a".repeat(64),
            declarations_hash: "b".repeat(64),
            project_snapshot_policy_hash: ryeos_state::objects::canonical_value_digest(
                &policy.to_value(),
            )
            .unwrap(),
            roots: ["absent", "empty", "runtime"]
                .into_iter()
                .map(|name| WorkspaceOutputRoot {
                    name: name.to_owned(),
                    path: format!("products/{name}"),
                    storage,
                    declared_bounds: bounds.clone(),
                    effective_bounds: bounds.clone(),
                })
                .collect(),
            products: Vec::new(),
            partition_identity: "0".repeat(64),
            capture_policy_digest: "0".repeat(64),
        };
        partition.partition_identity = partition.derived_partition_identity().unwrap();
        partition.capture_policy_digest = partition.derived_capture_policy_digest(policy).unwrap();
        partition.validate().unwrap();
        partition
    }

    fn policy(patterns: Vec<String>) -> ProjectSnapshotPolicy {
        let matcher = IgnoreMatcher::from_config(&IgnoreConfig { patterns }).unwrap();
        ProjectSnapshotPolicy::from_matcher(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            &matcher,
        )
        .unwrap()
    }

    #[test]
    fn both_storage_tiers_preserve_links_modes_empty_and_absent_roots() {
        for storage in [ProductStorage::Content, ProductStorage::LargeContent] {
            let temporary = tempfile::tempdir().unwrap();
            let state = ryeos_state::StateDb::open(
                temporary.path(),
                std::sync::Arc::new(ryeos_state::TrustStore::new()),
            )
            .unwrap();
            let authority = state.pinned_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            let mut stage = authority
                .require_recovery()
                .unwrap()
                .begin_staged_cas_roots_admitted(&guard, "workspace-output-test")
                .unwrap();
            let input = tempfile::tempdir().unwrap();
            let project = lillux::PinnedDirectory::open(input.path())
                .unwrap()
                .unwrap();
            let products = project.create_child(OsStr::new("products"), 0o755).unwrap();
            let empty = products.create_child(OsStr::new("empty"), 0o755).unwrap();
            // An all-excluded directory is empty in the admitted namespace,
            // not a required-product failure at an intermediate checkpoint.
            empty
                .create_child(OsStr::new("__pycache__"), 0o755)
                .unwrap();
            let runtime = products.create_child(OsStr::new("runtime"), 0o755).unwrap();
            runtime
                .create_child(OsStr::new("empty-child"), 0o755)
                .unwrap();
            let mut executable = runtime
                .open_regular_create(OsStr::new("program"), true, false, 0o755)
                .unwrap();
            executable.write_all(b"#!/bin/false\n").unwrap();
            executable.sync_all().unwrap();
            runtime
                .create_symlink(OsStr::new("alias"), b"program")
                .unwrap();
            let policy = policy(vec!["__pycache__/".to_owned()]);
            let partition = partition(storage, &policy);
            let isolation = ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring();
            let outputs = capture_native_workspace_outputs(
                &authority, &guard, &mut stage, &project, &partition, &policy, &isolation,
            )
            .unwrap();
            assert_eq!(outputs["absent"], WorkspaceOutputCaptureState::Absent);
            assert_eq!(
                outputs["empty"],
                WorkspaceOutputCaptureState::EmptyDirectory
            );
            assert!(matches!(
                outputs["runtime"],
                WorkspaceOutputCaptureState::Captured { .. }
            ));

            // Construct the source proof through real bounded CAS snapshot
            // verification, not a fake materialization constructor.
            let cas = authority.cas_store().unwrap();
            let policy_hash = stage
                .store_object_admitted(&guard, &cas, &policy.to_value())
                .unwrap();
            let tree = ryeos_state::objects::ProjectTree {
                files: BTreeMap::new(),
            };
            let tree_hash = stage
                .store_object_admitted(&guard, &cas, &tree.to_value())
                .unwrap();
            let snapshot = ryeos_state::objects::ProjectSnapshot {
                project_tree_hash: tree_hash,
                effective_policy_hash: policy_hash,
                message: None,
                parent_hashes: Vec::new(),
                created_at: "2026-09-08T00:00:00Z".to_owned(),
                source: "workspace-output-primitive-test".to_owned(),
            };
            let snapshot_hash = stage
                .store_object_admitted(&guard, &cas, &snapshot.to_value())
                .unwrap();
            let closure =
                ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
                    &cas,
                    &snapshot_hash,
                )
                .unwrap();
            let target = tempfile::tempdir().unwrap();
            let source = ryeos_state::project_materialization::PinnedProjectMaterialization::verify_from_closure(&authority, &guard, &closure, target.path()).unwrap();
            let mut capture = WorkspaceOutputCapture {
                schema: WORKSPACE_OUTPUT_CAPTURE_SCHEMA.to_owned(),
                kind: WORKSPACE_OUTPUT_CAPTURE_KIND.to_owned(),
                producer_chain_root_id: "T-primitive-root".to_owned(),
                producer_thread_id: "T-primitive-terminal".to_owned(),
                admitted_launch_capsule_hash: "c".repeat(64),
                base_project_snapshot_hash: snapshot_hash.clone(),
                result_project_snapshot_hash: snapshot_hash.clone(),
                partition,
                outputs,
            };
            capture.validate().unwrap();
            let budget = PrivateMaterializationBudget::new(4096);
            capture.result_project_snapshot_hash = "d".repeat(64);
            assert!(
                restore_workspace_outputs_before_view(
                    &authority, &guard, &source, &capture, &policy, &budget
                )
                .is_err()
            );
            assert!(!target.path().join("products").exists());
            capture.result_project_snapshot_hash = snapshot_hash;
            restore_workspace_outputs_before_view(
                &authority, &guard, &source, &capture, &policy, &budget,
            )
            .unwrap();
            assert!(!target.path().join("products/absent").exists());
            assert!(target.path().join("products/empty").is_dir());
            assert!(!target.path().join("products/empty/__pycache__").exists());
            assert!(target.path().join("products/runtime/empty-child").is_dir());
            let restored = source.try_clone_root().unwrap();
            let restored_runtime = open_output_directory(&restored, "products/runtime")
                .unwrap()
                .unwrap();
            assert_eq!(
                restored_runtime
                    .read_symlink_target(OsStr::new("alias"), 64)
                    .unwrap()
                    .unwrap(),
                b"program"
            );
            let file = restored_runtime
                .open_regular(OsStr::new("program"), false)
                .unwrap()
                .unwrap();
            assert_eq!(
                lillux::normalized_portable_regular_mode(&file.metadata().unwrap()).unwrap(),
                0o755
            );
            // Restored outputs don't relax ordinary source equality, and a
            // second restore cannot overwrite an existing mutable generation.
            assert!(source.ensure_path_binding().is_err());
            assert!(
                restore_workspace_outputs_before_view(
                    &authority, &guard, &source, &capture, &policy, &budget
                )
                .is_err()
            );
        }
    }

    #[test]
    fn frozen_delta_preserves_lower_outputs_and_applies_whiteouts_links_and_empty_dirs() {
        use ryeos_isolation_protocol::{WorkspaceMutation, WorkspaceMutationKind as Mutation};
        for storage in [ProductStorage::Content, ProductStorage::LargeContent] {
            let temporary = tempfile::tempdir().unwrap();
            let state = ryeos_state::StateDb::open(
                temporary.path(),
                std::sync::Arc::new(ryeos_state::TrustStore::new()),
            )
            .unwrap();
            let authority = state.pinned_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            let mut stage = authority
                .require_recovery()
                .unwrap()
                .begin_staged_cas_roots_admitted(&guard, "output-delta-test")
                .unwrap();
            let lower_dir = tempfile::tempdir().unwrap();
            let lower = lillux::PinnedDirectory::open(lower_dir.path())
                .unwrap()
                .unwrap();
            let products = lower.create_child(OsStr::new("products"), 0o755).unwrap();
            let runtime = products.create_child(OsStr::new("runtime"), 0o755).unwrap();
            for name in ["keep", "erase"] {
                runtime
                    .open_regular_create(OsStr::new(name), true, false, 0o640)
                    .unwrap()
                    .write_all(b"lower bytes")
                    .unwrap();
            }
            let replaced = runtime.create_child(OsStr::new("replaced"), 0o755).unwrap();
            replaced
                .open_regular_create(OsStr::new("old"), true, false, 0o644)
                .unwrap()
                .write_all(b"old bytes")
                .unwrap();
            for name in ["dir-to-file", "dir-to-link"] {
                runtime
                    .create_child(OsStr::new(name), 0o755)
                    .unwrap()
                    .open_regular_create(OsStr::new("old"), true, false, 0o644)
                    .unwrap()
                    .write_all(b"old")
                    .unwrap();
            }
            for name in ["file-to-dir", "file-to-link", "cycle-right"] {
                runtime
                    .open_regular_create(OsStr::new(name), true, false, 0o644)
                    .unwrap()
                    .write_all(b"old")
                    .unwrap();
            }
            for name in ["link-to-file", "link-to-dir"] {
                runtime.create_symlink(OsStr::new(name), b"keep").unwrap();
            }
            runtime
                .create_symlink(OsStr::new("cycle-left"), b"cycle-right")
                .unwrap();
            let policy = policy(vec!["__pycache__/".to_owned()]);
            let partition = partition(storage, &policy);
            let isolation = ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring();
            let outputs = capture_native_workspace_outputs(
                &authority, &guard, &mut stage, &lower, &partition, &policy, &isolation,
            )
            .unwrap();
            let base = WorkspaceOutputCapture {
                schema: WORKSPACE_OUTPUT_CAPTURE_SCHEMA.to_owned(),
                kind: WORKSPACE_OUTPUT_CAPTURE_KIND.to_owned(),
                producer_chain_root_id: "T-delta-root".to_owned(),
                producer_thread_id: "T-delta-base".to_owned(),
                admitted_launch_capsule_hash: "c".repeat(64),
                base_project_snapshot_hash: "d".repeat(64),
                result_project_snapshot_hash: "e".repeat(64),
                partition: partition.clone(),
                outputs,
            };
            base.validate().unwrap();
            let upper_dir = tempfile::tempdir().unwrap();
            let upper = lillux::PinnedDirectory::open(upper_dir.path())
                .unwrap()
                .unwrap();
            let changed = upper
                .create_child(OsStr::new("products"), 0o755)
                .unwrap()
                .create_child(OsStr::new("runtime"), 0o755)
                .unwrap();
            changed
                .open_regular_create(OsStr::new("new"), true, false, 0o755)
                .unwrap()
                .write_all(b"new bytes")
                .unwrap();
            changed
                .create_symlink(OsStr::new("alias"), b"keep")
                .unwrap();
            let mutation = |path: &str, kind| WorkspaceMutation {
                path: path.to_owned(),
                kind,
                normalized_mode: None,
                size: None,
                content_hash: None,
                target: None,
            };
            let mut mutations = vec![
                mutation("products", Mutation::EnsureDirectory),
                mutation("products/runtime", Mutation::EnsureDirectory),
                mutation("products/runtime/__pycache__", Mutation::EnsureDirectory),
                mutation("products/runtime/erase", Mutation::DeletePath),
                mutation("products/runtime/new-empty", Mutation::EnsureDirectory),
                mutation("products/runtime/replaced", Mutation::OpaqueDirectory),
                WorkspaceMutation {
                    target: Some("keep".to_owned()),
                    ..mutation("products/runtime/alias", Mutation::UpsertSymlink)
                },
                WorkspaceMutation {
                    normalized_mode: Some(0o755),
                    size: Some(9),
                    content_hash: Some(lillux::cas::sha256_hex(b"new bytes")),
                    ..mutation("products/runtime/new", Mutation::UpsertRegular)
                },
            ];
            mutations.sort_by(|left, right| left.path.cmp(&right.path));
            let unchanged = apply_output_delta(
                &authority,
                &guard,
                &mut stage,
                &upper,
                &partition,
                &policy,
                Some(&base),
                &[],
            )
            .unwrap();
            assert_eq!(
                unchanged, base.outputs,
                "unchanged lower bytes must not disappear from an empty upper"
            );
            let next = apply_output_delta(
                &authority,
                &guard,
                &mut stage,
                &upper,
                &partition,
                &policy,
                Some(&base),
                &mutations,
            )
            .unwrap();
            let WorkspaceOutputCaptureState::Captured { manifest_hash, .. } = &next["runtime"]
            else {
                panic!("runtime capture missing")
            };
            let value = authority
                .cas_store()
                .unwrap()
                .get_object(manifest_hash)
                .unwrap()
                .unwrap();
            let entries = value["entries"].as_array().unwrap();
            let entry = |path: &str| entries.iter().find(|entry| entry["path"] == path).unwrap();
            assert_eq!(
                entry("keep")["blob_hash"],
                lillux::cas::sha256_hex(b"lower bytes")
            );
            // Existing external manifests preserve portable executable class,
            // not owner/group-specific Unix permission bits.
            assert_eq!(entry("keep")["mode"], 0o644);
            assert_eq!(entry("new")["mode"], 0o755);
            assert_eq!(entry("alias")["target"], "keep");
            assert_eq!(entry("replaced")["kind"], "dir");
            assert_eq!(entry("new-empty")["kind"], "dir");
            assert!(!entries.iter().any(|entry| matches!(
                entry["path"].as_str(),
                Some("erase" | "replaced/old" | "__pycache__")
            )));
            let hidden = apply_output_delta(
                &authority,
                &guard,
                &mut stage,
                &upper,
                &partition,
                &policy,
                Some(&base),
                &[mutation("products", Mutation::OpaqueDirectory)],
            )
            .unwrap();
            assert!(
                hidden
                    .values()
                    .all(|state| matches!(state, WorkspaceOutputCaptureState::Absent))
            );
            let mut corrupt = mutations.clone();
            corrupt
                .iter_mut()
                .find(|mutation| mutation.kind == Mutation::UpsertRegular)
                .unwrap()
                .content_hash = Some("f".repeat(64));
            assert!(
                apply_output_delta(
                    &authority,
                    &guard,
                    &mut stage,
                    &upper,
                    &partition,
                    &policy,
                    Some(&base),
                    &corrupt
                )
                .is_err()
            );
            let invalid_root = WorkspaceMutation {
                target: Some("elsewhere".to_owned()),
                ..mutation("products/runtime", Mutation::UpsertSymlink)
            };
            assert!(
                apply_output_delta(
                    &authority,
                    &guard,
                    &mut stage,
                    &upper,
                    &partition,
                    &policy,
                    Some(&base),
                    &[invalid_root]
                )
                .is_err()
            );
            let mut replacements = Vec::new();
            let invalid_ancestor = WorkspaceMutation {
                normalized_mode: Some(0o644),
                size: Some(0),
                content_hash: Some(lillux::cas::sha256_hex(b"")),
                ..mutation("products", Mutation::UpsertRegular)
            };
            assert!(
                apply_output_delta(
                    &authority,
                    &guard,
                    &mut stage,
                    &upper,
                    &partition,
                    &policy,
                    Some(&base),
                    &[invalid_ancestor]
                )
                .is_err()
            );
            for name in ["dir-to-file", "link-to-file"] {
                changed
                    .open_regular_create(OsStr::new(name), true, false, 0o644)
                    .unwrap()
                    .write_all(b"replacement")
                    .unwrap();
                replacements.push(WorkspaceMutation {
                    normalized_mode: Some(0o644),
                    size: Some(11),
                    content_hash: Some(lillux::cas::sha256_hex(b"replacement")),
                    ..mutation(&format!("products/runtime/{name}"), Mutation::UpsertRegular)
                });
            }
            for name in ["dir-to-link", "file-to-link"] {
                changed.create_symlink(OsStr::new(name), b"keep").unwrap();
                replacements.push(WorkspaceMutation {
                    target: Some("keep".into()),
                    ..mutation(&format!("products/runtime/{name}"), Mutation::UpsertSymlink)
                });
            }
            for name in ["file-to-dir", "link-to-dir"] {
                changed.create_child(OsStr::new(name), 0o755).unwrap();
                replacements.push(mutation(
                    &format!("products/runtime/{name}"),
                    Mutation::EnsureDirectory,
                ));
            }
            replacements.sort_by(|left, right| left.path.cmp(&right.path));
            let replaced = apply_output_delta(
                &authority,
                &guard,
                &mut stage,
                &upper,
                &partition,
                &policy,
                Some(&base),
                &replacements,
            )
            .unwrap();
            let WorkspaceOutputCaptureState::Captured { manifest_hash, .. } = &replaced["runtime"]
            else {
                panic!("replacement capture missing")
            };
            let value = authority
                .cas_store()
                .unwrap()
                .get_object(manifest_hash)
                .unwrap()
                .unwrap();
            let entries = value["entries"].as_array().unwrap();
            for (path, kind) in [
                ("dir-to-file", "file"),
                ("link-to-file", "file"),
                ("dir-to-link", "symlink"),
                ("file-to-link", "symlink"),
                ("file-to-dir", "dir"),
                ("link-to-dir", "dir"),
            ] {
                let entry = entries.iter().find(|entry| entry["path"] == path).unwrap();
                assert_eq!(entry["kind"], kind);
                assert!(!entries.iter().any(|entry| {
                    entry["path"]
                        .as_str()
                        .unwrap()
                        .starts_with(&format!("{path}/"))
                }));
            }
            changed
                .create_symlink(OsStr::new("cycle-right"), b"cycle-left")
                .unwrap();
            let cycle = WorkspaceMutation {
                target: Some("cycle-left".into()),
                ..mutation("products/runtime/cycle-right", Mutation::UpsertSymlink)
            };
            let error = apply_output_delta(
                &authority,
                &guard,
                &mut stage,
                &upper,
                &partition,
                &policy,
                Some(&base),
                &[cycle],
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("cycle"));
            upper
                .open_child_directory(OsStr::new("products"))
                .unwrap()
                .unwrap()
                .create_child(OsStr::new("empty"), 0o755)
                .unwrap();
            let fresh = apply_output_delta(
                &authority,
                &guard,
                &mut stage,
                &upper,
                &partition,
                &policy,
                None,
                &[
                    mutation("products", Mutation::EnsureDirectory),
                    mutation("products/empty", Mutation::EnsureDirectory),
                ],
            )
            .unwrap();
            assert!(matches!(
                fresh["empty"],
                WorkspaceOutputCaptureState::EmptyDirectory
            ));
            assert!(matches!(
                fresh["runtime"],
                WorkspaceOutputCaptureState::Absent
            ));
            assert!(matches!(
                fresh["absent"],
                WorkspaceOutputCaptureState::Absent
            ));
        }
    }

    #[test]
    fn policy_replacement_and_excluded_optional_ancestors_fail_closed() {
        let initial = policy(Vec::new());
        let mut partition = partition(ProductStorage::Content, &initial);
        assert!(validate_capture_policy(&partition, &policy(vec!["*.tmp".to_owned()])).is_err());
        let stricter = policy(vec!["products/".to_owned()]);
        partition.project_snapshot_policy_hash =
            ryeos_state::objects::canonical_value_digest(&stricter.to_value()).unwrap();
        partition.partition_identity = partition.derived_partition_identity().unwrap();
        partition.capture_policy_digest =
            partition.derived_capture_policy_digest(&stricter).unwrap();
        let matcher = validate_capture_policy(&partition, &stricter).unwrap();
        assert!(
            ryeos_state::ExternalCapturePolicy::new("products/absent".to_owned(), &matcher)
                .is_err()
        );
    }

    #[test]
    fn retained_manifest_enforces_exact_capture_depth_and_floor() {
        let root = partition(ProductStorage::Content, &policy(Vec::new()))
            .roots
            .remove(0);
        let matcher = IgnoreMatcher::from_config(&IgnoreConfig {
            patterns: Vec::new(),
        })
        .unwrap();
        use ryeos_state::objects::ExternalContentManifestEntryKind::{Dir, File};
        let mut shallow = root;
        shallow.effective_bounds.maximum_depth = 1;
        assert!(
            validate_manifest_entries(&shallow, 1, 1, [("file", File, Some(1))], &matcher).is_ok()
        );
        assert!(validate_manifest_entries(&shallow, 1, 0, [("dir", Dir, None)], &matcher).is_err());
        assert!(
            validate_manifest_entries(
                &shallow,
                1,
                1,
                [(".ryeos-quarantine.fixture", File, Some(1))],
                &matcher
            )
            .is_err()
        );
    }
}
