//! Per-launch execution workspace layout.
//!
//! RyeOS owns the generation and lifecycle. The selected signed isolation
//! adapter owns composition of `lower + upper` for the launched process.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use ryeos_state::objects::{ProjectFile, ProjectSnapshotPolicy, ProjectTree};

pub const LOWER_DIR: &str = "project";
pub const UPPER_DIR: &str = "upper";
pub const WORK_DIR: &str = "work";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLayout {
    pub root: PathBuf,
    pub lower: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
}

impl WorkspaceLayout {
    pub fn create(execution_root: &Path, workspace_id: &str) -> Result<Self> {
        validate_workspace_id(workspace_id)?;
        let root = execution_root.join(workspace_id);
        match std::fs::create_dir(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&root)?;
                if !metadata.file_type().is_dir() {
                    return Err(error)
                        .with_context(|| format!("adopt execution workspace {}", root.display()));
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reserve execution workspace {}", root.display()))
            }
        }
        let layout = Self::from_root(root);
        for path in [&layout.lower, &layout.upper, &layout.work] {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !std::fs::symlink_metadata(path)?.file_type().is_dir() {
                        return Err(error).with_context(|| {
                            format!("adopt workspace directory {}", path.display())
                        });
                    }
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("create workspace directory {}", path.display()))
                }
            }
        }
        for entry in std::fs::read_dir(&layout.root)? {
            let name = entry?.file_name();
            if !matches!(name.to_str(), Some(LOWER_DIR | UPPER_DIR | WORK_DIR)) {
                anyhow::bail!(
                    "execution workspace contains unexpected entry: {}",
                    layout.root.join(name).display()
                );
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&layout.root, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&layout.lower, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&layout.upper, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&layout.work, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(layout)
    }

    pub fn from_root(root: PathBuf) -> Self {
        Self {
            lower: root.join(LOWER_DIR),
            upper: root.join(UPPER_DIR),
            work: root.join(WORK_DIR),
            root,
        }
    }

    pub fn from_lower(lower: &Path) -> Result<Self> {
        if lower.file_name().and_then(|name| name.to_str()) != Some(LOWER_DIR) {
            anyhow::bail!(
                "runtime project path is not a workspace lower: {}",
                lower.display()
            );
        }
        let root = lower
            .parent()
            .ok_or_else(|| anyhow::anyhow!("workspace lower has no parent"))?
            .to_path_buf();
        let layout = Self::from_root(root);
        for path in [&layout.lower, &layout.upper, &layout.work] {
            if !path.is_dir() {
                anyhow::bail!("workspace component is missing: {}", path.display());
            }
        }
        Ok(layout)
    }
}

fn validate_workspace_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("invalid execution workspace id `{value}`");
    }
    Ok(())
}

/// Apply the normalized native-overlay delta to an immutable project tree.
/// Only changed regular bytes are streamed into CAS; unchanged object hashes
/// are retained verbatim.
pub fn apply_workspace_delta(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    staged_roots: &mut ryeos_state::StagedCasRootLease,
    upper: &lillux::PinnedDirectory,
    base_tree: &ProjectTree,
    policy: &ProjectSnapshotPolicy,
    mutations: &[ryeos_isolation_protocol::WorkspaceMutation],
) -> Result<Option<ProjectTree>> {
    authority.ensure_guard(guard)?;
    policy.validate()?;
    let matcher = policy.matcher()?;
    let mut next = base_tree.clone();
    for mutation in mutations {
        mutation.validate()?;
        let relative = mutation.path.as_str();
        ryeos_state::project_sync::validate_safe_relative_path(relative)?;
        let included = !ryeos_state::project_sync::is_project_snapshot_floor_excluded(relative)
            && !matcher.is_ignored(relative)
            && (policy.sync_scope != ryeos_state::project_sync::ProjectSyncScope::AiOnly
                || matches!(
                    ryeos_state::project_sync::classify_project_ai_path(relative, Some(&matcher)),
                    ryeos_state::project_sync::ProjectAiPathClass::Deployable(_)
                ));
        match mutation.kind {
            ryeos_isolation_protocol::WorkspaceMutationKind::DeletePath => {
                remove_path_and_descendants(&mut next, relative);
            }
            ryeos_isolation_protocol::WorkspaceMutationKind::EnsureDirectory => {
                next.files.remove(relative);
            }
            ryeos_isolation_protocol::WorkspaceMutationKind::OpaqueDirectory => {
                next.files.remove(relative);
                remove_descendants(&mut next, relative);
            }
            ryeos_isolation_protocol::WorkspaceMutationKind::UpsertRegular if included => {
                ryeos_state::project_sync::validate_project_manifest_path(
                    relative,
                    policy.sync_scope,
                    Some(&matcher),
                )?;
                let (parent, name) = open_mutation_parent(upper, relative)?;
                let file = parent.open_regular(name.as_ref(), false)?.ok_or_else(|| {
                    anyhow::anyhow!("workspace mutation file disappeared: {relative}")
                })?;
                let metadata = file.metadata()?;
                if !metadata.file_type().is_file() {
                    anyhow::bail!("workspace mutation is not a regular file: {relative}");
                }
                #[cfg(unix)]
                let observed_mode = {
                    use std::os::unix::fs::PermissionsExt as _;
                    ProjectFile::normalize_mode(metadata.permissions().mode())
                };
                #[cfg(not(unix))]
                let observed_mode = ProjectFile::REGULAR_MODE;
                if mutation.normalized_mode != Some(observed_mode) {
                    anyhow::bail!(
                        "workspace mutation mode changed after adapter freeze: {relative}"
                    );
                }
                let cas = authority.cas_store()?;
                let streamed = cas.put_blob_from_open_regular(file, &parent.path().join(&name))?;
                if mutation.size != Some(streamed.size)
                    || mutation.content_hash.as_deref() != Some(streamed.hash.as_str())
                {
                    anyhow::bail!(
                        "workspace mutation bytes differ from the quiesced adapter evidence: {relative}"
                    );
                }
                staged_roots.protect_blob_hash_admitted(guard, &streamed.hash)?;
                let object = ProjectFile {
                    blob_hash: streamed.hash,
                    size: streamed.size,
                    normalized_mode: observed_mode,
                };
                object.validate()?;
                let object_hash =
                    staged_roots.store_object_admitted(guard, &cas, &object.to_value())?;
                remove_descendants(&mut next, relative);
                next.files.insert(relative.to_string(), object_hash);
            }
            ryeos_isolation_protocol::WorkspaceMutationKind::UpsertRegular => {}
        }
    }
    ryeos_state::project_sync::validate_project_tree_paths(&next, policy)?;
    Ok((next != *base_tree).then_some(next))
}

fn open_mutation_parent(
    root: &lillux::PinnedDirectory,
    relative: &str,
) -> Result<(lillux::PinnedDirectory, std::ffi::OsString)> {
    let mut components = relative.split('/').collect::<Vec<_>>();
    let name = components
        .pop()
        .ok_or_else(|| anyhow::anyhow!("workspace mutation path is empty"))?;
    let mut parent = root.try_clone()?;
    for component in components {
        parent = parent
            .open_child_directory(component.as_ref())?
            .ok_or_else(|| anyhow::anyhow!("workspace mutation parent is missing: {relative}"))?;
    }
    Ok((parent, std::ffi::OsString::from(name)))
}

fn remove_path_and_descendants(tree: &mut ProjectTree, path: &str) {
    let descendant_prefix = format!("{path}/");
    tree.files
        .retain(|candidate, _| candidate != path && !candidate.starts_with(&descendant_prefix));
}

fn remove_descendants(tree: &mut ProjectTree, path: &str) {
    let descendant_prefix = format!("{path}/");
    tree.files
        .retain(|candidate, _| !candidate.starts_with(&descendant_prefix));
}
