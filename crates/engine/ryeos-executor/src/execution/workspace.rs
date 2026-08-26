//! Per-launch execution workspace layout.
//!
//! RyeOS owns the canonical project generation and lifecycle. A selected
//! signed isolation adapter may own opaque backend state, but the executor
//! consumes only its normalized mutation evidence.

use anyhow::Result;
use ryeos_state::objects::{ProjectFile, ProjectSnapshotPolicy, ProjectTree};

pub use ryeos_engine::execution_workspace::{BACKEND_STATE_DIR, PROJECT_DIR, WorkspaceLayout};

/// Apply a normalized adapter mutation set to an immutable project tree.
/// Only changed regular bytes are streamed into CAS; unchanged object hashes
/// are retained verbatim.
pub fn apply_workspace_delta(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    staged_roots: &mut ryeos_state::StagedCasRootLease,
    mutation_content: &lillux::PinnedDirectory,
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
                let (parent, name) = open_mutation_parent(mutation_content, relative)?;
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
