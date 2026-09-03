//! Redemption of one admitted adjacent-source closure for execution.
//!
//! This module never consults live item roots. It verifies the retained
//! binding and manifest, materializes only their CAS blobs, and binds the
//! result at the canonical source coordinate proved by the binding.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
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

/// Source is the only admitted layer allowed to shadow the corresponding live
/// project namespace. It may never shadow, or be shadowed by, an independently
/// admitted external realization.
pub(crate) fn validate_external_mount_separation(
    state: &ryeos_app::state::AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<()> {
    let Some(source) = admitted_source_mount(state, resolution)? else {
        return Ok(());
    };
    let source = Path::new(&source);
    for external in super::external_content::admitted_realization_mounts(resolution)? {
        let external = Path::new(&external);
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
) -> anyhow::Result<Option<BoundSourceClosure>> {
    bind_source_with(
        state,
        resolution,
        workspace,
        BindingMode::IsolationMount,
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
