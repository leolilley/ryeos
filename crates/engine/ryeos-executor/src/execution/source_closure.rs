//! Redemption of one admitted adjacent-source closure for execution.
//!
//! This module never consults live item roots. It verifies the retained
//! binding and manifest, materializes only their CAS blobs, and binds the
//! result at the execution coordinate selected by the consuming launch contract.
//! Authored source location is identity, not authority to modify the project.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::Context as _;

pub(crate) struct BoundSourceClosure {
    mounts: Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
    sealed_identity_env: String,
    execution_entry_path: PathBuf,
    execution_root: PathBuf,
    members: std::collections::BTreeSet<String>,
    source_directory: lillux::PinnedDirectory,
    _leases: Vec<std::fs::File>,
}

impl BoundSourceClosure {
    fn member_path(&self, relative: &str) -> anyhow::Result<String> {
        ryeos_engine::runtime::validate_source_member_path(relative)?;
        anyhow::ensure!(
            self.members.contains(relative),
            "source member is absent from the admitted file manifest: {relative}"
        );
        anyhow::ensure!(
            self.source_directory
                .open_pinned_regular_descendant(Path::new(relative), false)?
                .is_some(),
            "admitted source member is not a retained regular file: {relative}"
        );
        self.execution_root
            .join(relative)
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("source member execution coordinate is not UTF-8"))
    }
    pub(crate) fn mounts(&self) -> &[ryeos_engine::isolation::IsolationReadOnlyMountAuthority] {
        &self.mounts
    }

    pub(crate) fn sealed_identity_env(&self) -> &str {
        &self.sealed_identity_env
    }

    /// Workload-namespace coordinate, not a daemon-readable pathname. The
    /// mount need not exist in the daemon's namespace before process launch.
    pub(crate) fn execution_entry_path(&self) -> &Path {
        &self.execution_entry_path
    }

    /// Exact verified materialization retained by this binding and its lease.
    /// Daemon-side reads must use this authority, never reopen the workload's
    /// execution coordinate or look up the installed/live source again.
    pub(crate) fn source_directory(&self) -> &lillux::PinnedDirectory {
        &self.source_directory
    }
}

pub(crate) fn bind_prepared_source_members(
    plan: &mut ryeos_app::thread_lifecycle::PreparedItemPlan,
    source: Option<&BoundSourceClosure>,
) -> anyhow::Result<()> {
    plan.bind_source_members(|relative| {
        source
            .ok_or_else(|| {
                anyhow::anyhow!("source member argument has no admitted source closure")
            })?
            .member_path(relative)
    })
}

pub(crate) fn direct_source_placement(
    plan: &ryeos_app::thread_lifecycle::PreparedItemPlan,
    enforced: bool,
) -> SourceMountPlacement {
    source_member_placement(plan.consumes_source_members(), enforced)
}

fn source_member_placement(consumes_members: bool, enforced: bool) -> SourceMountPlacement {
    if enforced && consumes_members {
        SourceMountPlacement::ExecutionRuntime
    } else {
        SourceMountPlacement::Project
    }
}

/// Fold-back must exclude only content actually shadowing the project. Replay
/// uses the symbolic admitted plan, never the already-lowered spawn arguments.
pub(crate) fn capsule_source_placement(
    closure: &ryeos_state::objects::AdmittedExecutionClosure,
    enforced: bool,
) -> anyhow::Result<SourceMountPlacement> {
    let ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
        execution_plan, ..
    } = closure
    else {
        return Ok(SourceMountPlacement::Project);
    };
    let plan: ryeos_engine::contracts::ExecutionPlan =
        serde_json::from_value(execution_plan.clone())?;
    let consumes_members = plan.nodes.iter().any(|node| match node {
        ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } => {
            spec.args.iter().any(|arg| {
                matches!(
                    arg,
                    ryeos_engine::contracts::PlanArgument::AdmittedSourceMember { .. }
                )
            })
        }
        _ => false,
    });
    Ok(source_member_placement(consumes_members, enforced))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceMountPlacement {
    /// Direct source loaders retain their admitted logical project namespace.
    Project,
    /// Typed-entry consumers need code, not a shadow in the user's project.
    ExecutionRuntime,
}

impl SourceMountPlacement {
    fn destination(self, workspace: &Path, relative: &str) -> anyhow::Result<PathBuf> {
        ryeos_state::objects::validate_canonical_project_relative_path(relative)?;
        Ok(self.mount_root().root(Some(workspace))?.join(relative))
    }

    fn mount_root(self) -> ryeos_state::objects::ExternalContentMountRoot {
        match self {
            Self::Project => ryeos_state::objects::ExternalContentMountRoot::Project,
            Self::ExecutionRuntime => {
                ryeos_state::objects::ExternalContentMountRoot::ExecutionRuntime
            }
        }
    }

    fn relative_mount(
        self,
        binding: &ryeos_state::objects::EffectiveSourceBinding,
    ) -> anyhow::Result<String> {
        match self {
            Self::Project => logical_mount(binding),
            // This is a namespace partition within the existing execution
            // runtime, not another content authority or a configurable host
            // path. The complete binding, not a workload name, owns the leaf.
            Self::ExecutionRuntime => Ok(format!("source-closures/{}", binding.digest()?)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingMode {
    IsolationMount(SourceMountPlacement),
    PrivateWorkspace,
}

pub(crate) fn admitted_source_mount(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<Option<String>> {
    let authority = super::pinned_state_authority(state)?;
    let cas = authority.cas_store()?;
    let Some((binding, _manifest, _projection)) = retained_source_records(&cas, resolution)? else {
        return Ok(None);
    };
    Ok(Some(logical_mount(&binding)?))
}

/// Compare destinations within the selected mount root. Runtime source does
/// not shadow project code, even if their authored relative names coincide.
pub(crate) fn validate_external_mount_separation(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    placement: SourceMountPlacement,
) -> anyhow::Result<()> {
    let authority = super::pinned_state_authority(state)?;
    let cas = authority.cas_store()?;
    let Some((binding, _, _)) = retained_source_records(&cas, resolution)? else {
        return Ok(());
    };
    let source = placement.relative_mount(&binding)?;
    let source = Path::new(&source);
    let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
    else {
        return Ok(());
    };
    let realized =
        ryeos_engine::external_realization::RealizedExternalContentSet::from_value(value)?;
    for entry in realized
        .iter()
        .filter(|entry| entry.mount_root == placement.mount_root())
    {
        let external = Path::new(&entry.mount);
        if mount_destinations_overlap(source, external) {
            anyhow::bail!("admitted source and external realization destinations overlap");
        }
    }
    Ok(())
}

fn mount_destinations_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

pub(crate) fn bind_source(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    workspace: &Path,
    placement: SourceMountPlacement,
) -> anyhow::Result<Option<BoundSourceClosure>> {
    bind_source_with(
        state,
        resolution,
        workspace,
        BindingMode::IsolationMount(placement),
        None,
    )
}

pub(crate) fn bind_source_in_private_workspace_with_budget(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    workspace: &Path,
    budget: &super::external_content::PrivateMaterializationBudget,
) -> anyhow::Result<Option<BoundSourceClosure>> {
    bind_source_with(
        state,
        resolution,
        workspace,
        BindingMode::PrivateWorkspace,
        Some(budget),
    )
}

fn bind_source_with(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    workspace: &Path,
    mode: BindingMode,
    budget: Option<&super::external_content::PrivateMaterializationBudget>,
) -> anyhow::Result<Option<BoundSourceClosure>> {
    let authority = super::pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let Some((binding, manifest, projection)) = retained_source_records(&cas, resolution)? else {
        return Ok(None);
    };
    let placement = match mode {
        BindingMode::IsolationMount(placement) => placement,
        BindingMode::PrivateWorkspace => SourceMountPlacement::Project,
    };
    let mount = placement.relative_mount(&binding)?;
    let entry = logical_entry(&binding)?;
    let identity = serde_json::json!({
        "schema": projection.schema,
        "binding_hash": projection.binding_hash,
        "content_manifest_hash": projection.content_manifest_hash,
        "owner_key": projection.owner_key,
    });
    let sealed_identity_env = lillux::canonical_json(&identity)?;
    if sealed_identity_env.len() > 2048 {
        anyhow::bail!("admitted source identity exceeds its protected environment bound");
    }
    let files = manifest
        .entries
        .iter()
        .map(|entry| {
            let mode = match entry.mode {
                ryeos_state::objects::SourceFileMode::ReadOnly => 0o644,
                ryeos_state::objects::SourceFileMode::Executable => 0o755,
            };
            (
                entry.path.clone(),
                ryeos_state::objects::ProjectFile {
                    blob_hash: entry.blob_hash.clone(),
                    size: entry.size,
                    normalized_mode: mode,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let cache = super::cache::MaterializationCache::new(
        state
            .config
            .runtime_state_dir()
            .join("cache/source-closures"),
    );
    let generation = &projection.content_manifest_hash;
    let build = cache.generation_build_lock(generation)?;
    if cache
        .verify_completion_marker_for_files(&files, generation)
        .is_err()
    {
        cache.discard_generation(generation)?;
        let cache_root = cache.pinned_root()?;
        let staging_name = OsString::from(format!(
            "{generation}.staging.{}.{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let staging = cache_root.create_child(&staging_name, 0o700)?;
        let construction = (|| {
            for (relative, file) in &files {
                let content = cache.ensure_content_file(&cas, file)?;
                let (parent, name) = super::pinned_output_parent(&staging, relative)?;
                content.link_to(&parent, &name)?;
            }
            cache.publish_tree(&cache_root, &staging_name, &staging, generation)
        })();
        if construction.is_err() && staging.ensure_path_binding().is_ok() {
            let _ = staging.remove_contents_recursive().and_then(|()| {
                cache_root
                    .remove_empty_child_if_same(&staging_name, &staging)
                    .map(|_| ())
            });
        }
        construction?;
    }
    cache.verify_completion_marker_for_files(&files, generation)?;
    let lease = cache.generation_lease(generation)?;
    drop(build);
    let source_path = cache.cache_dir(generation);
    let source = lillux::PinnedDirectory::open(&source_path)?
        .ok_or_else(|| anyhow::anyhow!("admitted source generation disappeared"))?;
    let destination = placement.destination(workspace, &mount)?;
    let mounts = match mode {
        BindingMode::IsolationMount(placement) => vec![match placement {
            SourceMountPlacement::Project => {
                ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new(
                    source_path,
                    destination.clone(),
                    source.inherited_descriptor_authority()?,
                )
            }
            SourceMountPlacement::ExecutionRuntime => {
                ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new_execution_runtime(
                    source_path,
                    destination.clone(),
                    source.inherited_descriptor_authority()?,
                )
            }
        }],
        BindingMode::PrivateWorkspace => {
            publish_private_source(
                &source,
                workspace,
                &mount,
                &manifest,
                budget
                    .ok_or_else(|| anyhow::anyhow!("private source binding has no copy budget"))?,
            )?;
            Vec::new()
        }
    };
    authority.ensure_guard(&guard)?;
    Ok(Some(BoundSourceClosure {
        mounts,
        sealed_identity_env,
        execution_entry_path: destination.join(entry),
        execution_root: destination,
        members: manifest
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect(),
        source_directory: source,
        _leases: vec![lease],
    }))
}

fn retained_source_records(
    cas: &lillux::CasStore,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<
    Option<(
        ryeos_state::objects::EffectiveSourceBinding,
        ryeos_state::objects::SourceClosureManifest,
        ryeos_state::objects::EffectiveSourceClosureProjection,
    )>,
> {
    let Some(value) = resolution
        .composed
        .derived
        .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
    else {
        return Ok(None);
    };
    let projection = ryeos_state::objects::EffectiveSourceClosureProjection::from_value(value)?;
    let binding_value = cas
        .get_object(&projection.binding_hash)?
        .ok_or_else(|| anyhow::anyhow!("admitted source binding is missing"))?;
    let binding = ryeos_state::objects::EffectiveSourceBinding::from_value(&binding_value)?;
    let manifest_value = cas
        .get_object(&projection.content_manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("admitted source manifest is missing"))?;
    let manifest = ryeos_state::objects::SourceClosureManifest::from_value(&manifest_value)?;
    if binding.digest()? != projection.binding_hash
        || binding.content_manifest_hash != projection.content_manifest_hash
        || manifest.digest()? != projection.content_manifest_hash
        || binding.owner_key()? != projection.owner_key
    {
        anyhow::bail!("admitted source records contradict their effective projection");
    }
    binding.validate_content_manifest(&manifest)?;
    Ok(Some((binding, manifest, projection)))
}

fn logical_mount(binding: &ryeos_state::objects::EffectiveSourceBinding) -> anyhow::Result<String> {
    let directory = binding
        .kind_ceiling
        .schema_document
        .get("location")
        .and_then(|location| location.get("directory"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("retained source kind has no logical directory"))?;
    ryeos_state::objects::validate_canonical_project_relative_path(directory)?;
    let mut path = PathBuf::from(".ai").join(directory);
    match &binding.logical_binding {
        ryeos_state::objects::SourceLogicalBinding::Tool { .. } => {
            if let Some((namespace, _)) = binding.owner.logical_item_key.split_once('/') {
                path.push(namespace);
            }
        }
        ryeos_state::objects::SourceLogicalBinding::ToolDirectory { root, .. } => {
            path.push(root);
        }
        ryeos_state::objects::SourceLogicalBinding::Worker { root, .. } => {
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
    ryeos_state::objects::validate_canonical_project_relative_path(&value)?;
    Ok(value)
}

fn logical_entry(binding: &ryeos_state::objects::EffectiveSourceBinding) -> anyhow::Result<&str> {
    Ok(match &binding.logical_binding {
        ryeos_state::objects::SourceLogicalBinding::Tool { root_entry, .. } => root_entry,
        ryeos_state::objects::SourceLogicalBinding::ToolDirectory { root_entry, .. } => root_entry,
        ryeos_state::objects::SourceLogicalBinding::Worker { entry, .. } => entry,
    })
}

fn publish_private_source(
    source: &lillux::PinnedDirectory,
    workspace: &Path,
    mount: &str,
    manifest: &ryeos_state::objects::SourceClosureManifest,
    budget: &super::external_content::PrivateMaterializationBudget,
) -> anyhow::Result<()> {
    let workspace = lillux::PinnedDirectory::open(workspace)?
        .ok_or_else(|| anyhow::anyhow!("private source workspace is absent"))?;
    let (parent_path, name) = Path::new(mount)
        .parent()
        .zip(Path::new(mount).file_name())
        .ok_or_else(|| anyhow::anyhow!("source mount has no parent or name"))?;
    let mut parent = workspace.try_clone()?;
    for component in parent_path.components() {
        let std::path::Component::Normal(component) = component else {
            anyhow::bail!("source mount is not normalized");
        };
        parent = parent.open_or_create_child(component, 0o700)?;
    }
    if let Some(existing) = parent.open_entry(name, false)? {
        match existing {
            lillux::PinnedDirectoryEntry::Directory(directory) => {
                directory.remove_contents_recursive()?;
                if !parent.remove_empty_child_if_same(name, &directory)? {
                    anyhow::bail!("private source shadow remained non-empty");
                }
            }
            lillux::PinnedDirectoryEntry::Regular(file) => {
                parent.remove_if_same(name, &file)?;
            }
        }
    }
    let target = parent.create_child(name, 0o700)?;
    for entry in &manifest.entries {
        let (output, filename) = super::pinned_output_parent(&target, &entry.path)?;
        let mode = match entry.mode {
            // The CAS materializer accepts the same normalized regular-file
            // modes used by project capture. Enforced execution remounts this
            // tree read-only; disabled execution receives an independent
            // daemon-private copy, so child writes cannot mutate retained CAS
            // bytes or any live project tree.
            ryeos_state::objects::SourceFileMode::ReadOnly => 0o644,
            ryeos_state::objects::SourceFileMode::Executable => 0o755,
        };
        let (source_parent, source_name) = open_source_parent(source, &entry.path)?;
        let source_file = source_parent
            .open_regular(&source_name, false)?
            .ok_or_else(|| anyhow::anyhow!("admitted source cache file disappeared"))?;
        budget
            .materialize_regular(&output, &filename, &source_file, entry.size, mode)
            .with_context(|| format!("materialize admitted source file {}", entry.path))?;
    }
    target.sync_tree()?;
    verify_private_source(&target, manifest)?;
    Ok(())
}

fn verify_private_source(
    root: &lillux::PinnedDirectory,
    manifest: &ryeos_state::objects::SourceClosureManifest,
) -> anyhow::Result<()> {
    let expected = manifest
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut observed = Vec::with_capacity(expected.len());
    verify_private_source_directory(root, "", &expected, &mut observed)?;
    observed.sort();
    if observed.iter().map(String::as_str).collect::<Vec<_>>()
        != expected.keys().copied().collect::<Vec<_>>()
    {
        anyhow::bail!("private admitted source has missing or extra files");
    }
    Ok(())
}

fn verify_private_source_directory(
    directory: &lillux::PinnedDirectory,
    prefix: &str,
    expected: &BTreeMap<&str, &ryeos_state::objects::SourceClosureFile>,
    observed: &mut Vec<String>,
) -> anyhow::Result<()> {
    for actual in directory.entries_no_follow_bounded(expected.len().saturating_add(1))? {
        let name = actual
            .name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("private admitted source has a non-UTF-8 entry"))?;
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        match actual.entry_type {
            lillux::PinnedEntryType::Directory => {
                let descendant_prefix = format!("{path}/");
                if !expected
                    .keys()
                    .any(|candidate| candidate.starts_with(&descendant_prefix))
                {
                    anyhow::bail!("private admitted source has unexpected directory {path}");
                }
                let child = directory
                    .open_child_directory(&actual.name)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("private admitted source directory {path} disappeared")
                    })?;
                verify_private_source_directory(&child, &path, expected, observed)?;
            }
            lillux::PinnedEntryType::Regular => {
                let entry = expected.get(path.as_str()).ok_or_else(|| {
                    anyhow::anyhow!("private admitted source has unexpected file {path}")
                })?;
                let mut file = directory
                    .open_regular(&actual.name, false)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("private admitted source file {path} disappeared")
                    })?;
                let (digest, metadata) =
                    lillux::digest_open_regular_file_stable_exact(&mut file, entry.size)?;
                let expected_mode = match entry.mode {
                    ryeos_state::objects::SourceFileMode::ReadOnly => 0o644,
                    ryeos_state::objects::SourceFileMode::Executable => 0o755,
                };
                if digest != entry.blob_hash
                    || lillux::normalized_portable_regular_mode(&metadata)? != expected_mode
                {
                    anyhow::bail!("private admitted source file {path} failed verification");
                }
                observed.push(path);
            }
            _ => anyhow::bail!("private admitted source contains unsupported entry {path}"),
        }
    }
    Ok(())
}

fn open_source_parent(
    root: &lillux::PinnedDirectory,
    relative: &str,
) -> anyhow::Result<(lillux::PinnedDirectory, OsString)> {
    let mut components = relative.split('/').peekable();
    let mut parent = root.try_clone()?;
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok((parent, OsString::from(component)));
        }
        parent = parent
            .open_child_directory(OsStr::new(component))?
            .ok_or_else(|| anyhow::anyhow!("admitted source cache directory disappeared"))?;
    }
    anyhow::bail!("admitted source path is empty")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory_binding() -> ryeos_state::objects::EffectiveSourceBinding {
        ryeos_state::objects::EffectiveSourceBinding {
            schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
            kind: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_KIND.to_owned(),
            owner: ryeos_state::objects::SourceOwnerIdentity {
                canonical_ref: "tool:ryeos/development/authoring-environment-production/runtime"
                    .to_owned(),
                item_kind: "tool".to_owned(),
                source_space: ryeos_state::objects::SourceSpaceIdentity::Bundle,
                source_root: ryeos_state::objects::SourceRootIdentity::Bundle {
                    name: "standard".to_owned(),
                },
                root_source_content_digest: "a".repeat(64),
                root_raw_content_digest: "b".repeat(64),
                signer_fingerprint: "c".repeat(64),
                logical_item_key: "ryeos/development/authoring-environment-production/runtime"
                    .to_owned(),
            },
            kind_ceiling: ryeos_state::objects::SignedKindSourceCeiling {
                schema_ref: "kind:tool".to_owned(),
                source_content_digest: "d".repeat(64),
                raw_content_digest: "e".repeat(64),
                signer_fingerprint: "f".repeat(64),
                signature_header: "signed".to_owned(),
                schema_body: "kind: tool\n".to_owned(),
                schema_document: serde_json::json!({"location": {"directory": "tools"}}),
                normalized_declaration: serde_json::json!({}),
                root_kind_format: serde_json::json!({}),
                root_signature_envelope: serde_json::json!({}),
            },
            content_manifest_hash: "1".repeat(64),
            testimony: ryeos_state::objects::SourceTestimonyProof::OwnerSignedFiles {
                signer_fingerprint: "c".repeat(64),
                file_count: 1,
                entries_digest: "2".repeat(64),
            },
            execution_policy: ryeos_state::objects::SourceExecutionPolicyIdentity::Executor {
                declarer_ref: "tool:ryeos/core/runtimes/python/function".to_owned(),
                signer_fingerprint: "3".repeat(64),
                source_content_digest: "4".repeat(64),
                raw_content_digest: "5".repeat(64),
                policy_digest: "6".repeat(64),
                chain_digest: "7".repeat(64),
            },
            logical_binding: ryeos_state::objects::SourceLogicalBinding::ToolDirectory {
                loader_roots: vec![ryeos_state::objects::SourceLoaderRoot::ItemDirectory],
                root: "ryeos/development/authoring-environment-production".to_owned(),
                root_entry: "runtime.yaml".to_owned(),
            },
        }
    }

    #[test]
    fn directory_source_redeems_at_its_exact_authored_coordinate() {
        let binding = directory_binding();
        assert_eq!(
            logical_mount(&binding).unwrap(),
            ".ai/tools/ryeos/development/authoring-environment-production"
        );
        assert_eq!(logical_entry(&binding).unwrap(), "runtime.yaml");
        assert_ne!(logical_mount(&binding).unwrap(), ".ai/tools/ryeos");
    }

    #[test]
    fn runtime_source_coordinate_does_not_require_or_modify_project_namespace() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("edited.rs"), b"candidate edits").unwrap();
        let relative = format!("source-closures/{}", "a".repeat(64));
        let runtime = SourceMountPlacement::ExecutionRuntime
            .destination(workspace.path(), &relative)
            .unwrap();
        assert_eq!(
            runtime,
            Path::new(ryeos_state::objects::EXECUTION_RUNTIME_REALIZATIONS_ROOT).join(&relative)
        );
        assert!(!runtime.starts_with(workspace.path()));
        assert!(!workspace.path().join(".ai").exists());
        assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 1);
        assert_eq!(
            std::fs::read(workspace.path().join("edited.rs")).unwrap(),
            b"candidate edits"
        );
        assert!(mount_destinations_overlap(
            &runtime,
            &runtime.join("nested")
        ));
        assert!(mount_destinations_overlap(
            &runtime,
            runtime.parent().unwrap()
        ));
        assert!(!mount_destinations_overlap(
            &runtime,
            &workspace.path().join(&relative)
        ));
        assert_eq!(
            SourceMountPlacement::Project
                .destination(workspace.path(), ".ai/tools/example")
                .unwrap(),
            workspace.path().join(".ai/tools/example")
        );
        for placement in [
            SourceMountPlacement::Project,
            SourceMountPlacement::ExecutionRuntime,
        ] {
            assert!(
                placement
                    .destination(workspace.path(), "../escape")
                    .is_err()
            );
            assert!(
                placement
                    .destination(workspace.path(), "/absolute")
                    .is_err()
            );
        }
    }

    #[test]
    fn source_member_retained_closure_has_no_project_shadow() {
        use ryeos_state::objects::{AdmittedDirectCommandClosure, AdmittedExecutionClosure};
        let mut closure = AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan: serde_json::json!({
                "plan_id":"test", "root_executor_id":"@subprocess", "root_ref":"tool:test/run", "item_kind":"tool",
                "nodes":[{"node_type":"dispatch_subprocess", "id":"spawn", "spec":{
                    "cmd":"/python", "args":[{"kind":"admitted_source_member", "relative_path":"lib/run.py"}]
                }, "executor_chain":[]}],
                "entrypoint":"spawn", "capabilities":{"requires_model":false,"requires_subprocess":true,"requires_network":false,"custom":[]},
                "materialization_requirements":[], "network_authority_ceiling":"node_policy", "filesystem_authority_ceiling":"node_policy",
                "target_requirement":null, "resource_authority_ceiling":"node_policy", "cache_key":"test", "executor_authorities":[]
            }),
            protocol_descriptor_document: String::new(),
            command: AdmittedDirectCommandClosure::NodePolicy,
            admitted_project_root: Some("/project".into()),
        };
        let retained: AdmittedExecutionClosure =
            serde_json::from_value(serde_json::to_value(&closure).unwrap()).unwrap();
        assert_eq!(
            capsule_source_placement(&retained, true).unwrap(),
            SourceMountPlacement::ExecutionRuntime
        );
        assert_eq!(
            capsule_source_placement(&retained, false).unwrap(),
            SourceMountPlacement::Project
        );
        if let AdmittedExecutionClosure::DirectItemExecutor { execution_plan, .. } = &mut closure {
            execution_plan["nodes"][0]["spec"]["args"] =
                serde_json::json!([{"kind":"literal","value":".ai/tools/test/run.py"}]);
        }
        assert_eq!(
            capsule_source_placement(&closure, true).unwrap(),
            SourceMountPlacement::Project
        );
    }

    #[test]
    fn source_member_placement_matches_actual_delivery() {
        assert_eq!(
            source_member_placement(true, true),
            SourceMountPlacement::ExecutionRuntime
        );
        assert_eq!(
            source_member_placement(true, false),
            SourceMountPlacement::Project
        );
        assert_eq!(
            source_member_placement(false, true),
            SourceMountPlacement::Project
        );
        assert_eq!(
            source_member_placement(false, false),
            SourceMountPlacement::Project
        );
    }

    #[test]
    fn bound_source_keeps_daemon_authority_separate_from_execution_coordinate() {
        let cache = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(cache.path().join("baseline.toml"), b"captured = true\n").unwrap();
        std::fs::write(cache.path().join("unlisted.py"), b"not admitted").unwrap();
        std::fs::create_dir(cache.path().join("directory")).unwrap();
        std::os::unix::fs::symlink("baseline.toml", cache.path().join("alias")).unwrap();
        let bound = BoundSourceClosure {
            mounts: Vec::new(),
            sealed_identity_env: "{}".to_owned(),
            execution_entry_path: workspace.path().join("not-mounted/profile.json"),
            execution_root: workspace.path().join("not-mounted"),
            members: ["baseline.toml", "directory", "alias"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            source_directory: lillux::PinnedDirectory::open(cache.path())
                .unwrap()
                .unwrap(),
            _leases: Vec::new(),
        };
        assert!(!bound.execution_entry_path().parent().unwrap().exists());
        assert_eq!(
            bound.member_path("baseline.toml").unwrap(),
            workspace
                .path()
                .join("not-mounted/baseline.toml")
                .to_str()
                .unwrap()
        );
        assert!(bound.member_path("missing.py").is_err());
        assert!(bound.member_path("unlisted.py").is_err());
        assert!(bound.member_path("directory").is_err());
        assert!(bound.member_path("alias").is_err());
        assert!(bound.member_path("../baseline.toml").is_err());
        assert!(!workspace.path().join("not-mounted").exists());
        let file = bound
            .source_directory()
            .open_pinned_regular_descendant(Path::new("baseline.toml"), false)
            .unwrap()
            .unwrap();
        assert_eq!(file.read_bounded(64).unwrap(), b"captured = true\n");
        assert!(
            bound
                .source_directory()
                .open_pinned_regular_descendant(Path::new("../baseline.toml"), false)
                .is_err()
        );
    }

    #[test]
    fn source_and_external_mounts_refuse_exact_or_nested_overlap() {
        let source = Path::new(".ai/tools/arc");
        assert!(mount_destinations_overlap(
            source,
            Path::new(".ai/tools/arc")
        ));
        assert!(mount_destinations_overlap(
            source,
            Path::new(".ai/tools/arc/vendor")
        ));
        assert!(mount_destinations_overlap(source, Path::new(".ai/tools")));
        assert!(!mount_destinations_overlap(
            source,
            Path::new("vendor/simulator")
        ));
    }

    #[test]
    fn private_source_shadow_contains_only_retained_cas_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cas_root = dir.path().join("cas");
        let source_cache = dir.path().join("source-cache");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(source_cache.join("lib")).unwrap();
        std::fs::write(source_cache.join("solve.py"), b"sealed solve").unwrap();
        std::fs::write(source_cache.join("lib/helper.py"), b"sealed helper").unwrap();
        std::fs::create_dir_all(workspace.join(".ai/tools/arc")).unwrap();
        std::fs::write(workspace.join(".ai/tools/arc/solve.py"), b"live").unwrap();
        std::fs::write(workspace.join(".ai/tools/arc/ambient.py"), b"ambient").unwrap();
        let cas = lillux::CasStore::new(cas_root);
        let solve = cas.store_blob(b"sealed solve").unwrap();
        let helper = cas.store_blob(b"sealed helper").unwrap();
        let manifest = ryeos_state::objects::SourceClosureManifest::new(
            vec![ryeos_state::objects::LogicalSourceRoot {
                id: "source".to_owned(),
            }],
            vec![
                ryeos_state::objects::SourceClosureFile {
                    root: "source".to_owned(),
                    path: "solve.py".to_owned(),
                    blob_hash: solve,
                    size: 12,
                    mode: ryeos_state::objects::SourceFileMode::ReadOnly,
                },
                ryeos_state::objects::SourceClosureFile {
                    root: "source".to_owned(),
                    path: "lib/helper.py".to_owned(),
                    blob_hash: helper,
                    size: 13,
                    mode: ryeos_state::objects::SourceFileMode::ReadOnly,
                },
            ],
        )
        .unwrap();

        let source_cache = lillux::PinnedDirectory::open(&source_cache)
            .unwrap()
            .unwrap();
        let budget = super::super::external_content::PrivateMaterializationBudget::new(u64::MAX);
        publish_private_source(
            &source_cache,
            &workspace,
            ".ai/tools/arc",
            &manifest,
            &budget,
        )
        .unwrap();

        assert_eq!(
            std::fs::read(workspace.join(".ai/tools/arc/solve.py")).unwrap(),
            b"sealed solve"
        );
        assert_eq!(
            std::fs::read(workspace.join(".ai/tools/arc/lib/helper.py")).unwrap(),
            b"sealed helper"
        );
        assert!(!workspace.join(".ai/tools/arc/ambient.py").exists());
    }
}
