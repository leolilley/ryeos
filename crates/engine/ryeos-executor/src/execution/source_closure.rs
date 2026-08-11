//! Redemption of one admitted adjacent-source closure for execution.
//!
//! This module never consults live item roots. It verifies the retained
//! binding and manifest, materializes only their CAS blobs, and binds the
//! result at the canonical source coordinate proved by the binding.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

pub(crate) struct BoundSourceClosure {
    mounts: Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
    sealed_identity_env: String,
    entry_path: PathBuf,
    _leases: Vec<std::fs::File>,
}

impl BoundSourceClosure {
    pub(crate) fn mounts(&self) -> &[ryeos_engine::isolation::IsolationReadOnlyMountAuthority] {
        &self.mounts
    }

    pub(crate) fn sealed_identity_env(&self) -> &str {
        &self.sealed_identity_env
    }

    pub(crate) fn entry_path(&self) -> &Path {
        &self.entry_path
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingMode {
    IsolationMount,
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

pub(crate) fn bind_source_for_execution(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    workspace: &Path,
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> anyhow::Result<Option<BoundSourceClosure>> {
    let daemon_owned_workspace = matches!(
        project_authority,
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. }
            | ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                realization: ryeos_state::objects::PinnedProjectRealization::Cow { .. },
                ..
            }
    );
    let mode = if !state.isolation.is_enforced() && daemon_owned_workspace {
        BindingMode::PrivateWorkspace
    } else {
        BindingMode::IsolationMount
    };
    bind_source_with(state, resolution, workspace, mode)
}

pub(crate) fn bind_source(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    workspace: &Path,
) -> anyhow::Result<Option<BoundSourceClosure>> {
    bind_source_with(state, resolution, workspace, BindingMode::IsolationMount)
}

pub(crate) fn bind_source_in_private_workspace(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    workspace: &Path,
) -> anyhow::Result<Option<BoundSourceClosure>> {
    bind_source_with(state, resolution, workspace, BindingMode::PrivateWorkspace)
}

fn bind_source_with(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    workspace: &Path,
    mode: BindingMode,
) -> anyhow::Result<Option<BoundSourceClosure>> {
    let authority = super::pinned_state_authority(state)?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let Some((binding, manifest, projection)) = retained_source_records(&cas, resolution)? else {
        return Ok(None);
    };
    let mount = logical_mount(&binding)?;
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
    let destination = workspace.join(&mount);
    let mounts = match mode {
        BindingMode::IsolationMount => vec![
            ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new(
                source_path,
                destination.clone(),
                source.try_clone_descriptor()?,
            ),
        ],
        BindingMode::PrivateWorkspace => {
            publish_private_source(&cas, workspace, &mount, &manifest)?;
            Vec::new()
        }
    };
    authority.ensure_guard(&guard)?;
    Ok(Some(BoundSourceClosure {
        mounts,
        sealed_identity_env,
        entry_path: destination.join(entry),
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
        ryeos_state::objects::SourceLogicalBinding::Worker { entry, .. } => entry,
    })
}

fn publish_private_source(
    cas: &lillux::CasStore,
    workspace: &Path,
    mount: &str,
    manifest: &ryeos_state::objects::SourceClosureManifest,
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
            ryeos_state::objects::SourceFileMode::ReadOnly => 0o444,
            ryeos_state::objects::SourceFileMode::Executable => 0o555,
        };
        let observed = cas
            .materialize_blob_to_new_regular(&entry.blob_hash, &output, &filename, mode)
            .with_context(|| format!("materialize admitted source file {}", entry.path))?;
        if observed != entry.size {
            anyhow::bail!("materialized source file size contradicts its manifest");
        }
    }
    target.sync_tree()?;
    Ok(())
}
