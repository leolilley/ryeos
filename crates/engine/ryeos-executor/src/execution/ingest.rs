use std::fs;
use std::path::Path;

use anyhow::Result;

use ryeos_state::objects::{ProjectFile, ProjectSnapshotPolicy, ProjectTree};

/// Capture one complete project tree with descriptor-relative traversal and
/// streaming blob ingestion. The policy is immutable input to this capture.
pub fn ingest_project_tree(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    project_root: &lillux::PinnedDirectory,
    policy: &ProjectSnapshotPolicy,
) -> Result<ProjectTree> {
    authority.ensure_guard(guard)?;
    policy.validate()?;
    let matcher = policy.matcher()?;
    let cas = authority.cas_store()?;
    let mut files = std::collections::BTreeMap::new();
    project_root.visit_regular_files(
        |relative, is_directory| {
            let rel = canonical_relative_path(relative)?;
            if ryeos_state::project_sync::is_project_snapshot_floor_excluded(&rel)
                || matcher.is_ignored(&rel)
            {
                return Ok(true);
            }
            if policy.sync_scope == ryeos_state::project_sync::ProjectSyncScope::AiOnly {
                return Ok(!matches!(
                    ryeos_state::project_sync::classify_project_ai_path(&rel, Some(&matcher)),
                    ryeos_state::project_sync::ProjectAiPathClass::Deployable(_)
                ));
            }
            let _ = is_directory;
            Ok(false)
        },
        |relative, file| {
            let rel = canonical_relative_path(relative)?;
            ryeos_state::project_sync::validate_project_manifest_path(
                &rel,
                policy.sync_scope,
                Some(&matcher),
            )?;
            let streamed =
                cas.put_blob_from_open_regular(file, &project_root.path().join(relative))?;
            let project_file = ProjectFile {
                blob_hash: streamed.hash,
                size: streamed.size,
                normalized_mode: streamed.normalized_mode,
            };
            project_file.validate()?;
            let file_hash = cas.store_object(&project_file.to_value())?;
            if files.insert(rel.clone(), file_hash).is_some() {
                anyhow::bail!("duplicate canonical project path during capture: {rel}");
            }
            Ok(())
        },
    )?;
    let tree = ProjectTree { files };
    ryeos_state::project_sync::validate_project_tree_paths(&tree, policy)?;
    Ok(tree)
}

fn canonical_relative_path(relative: &Path) -> Result<String> {
    let value = relative
        .to_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project-relative path '{}' is not valid UTF-8",
                relative.display()
            )
        })?
        .replace('\\', "/");
    ryeos_state::project_sync::validate_safe_relative_path(&value)?;
    Ok(value)
}

pub fn materialize_project_file(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    object_hash: &str,
    target_path: &Path,
) -> Result<()> {
    authority.ensure_guard(guard)?;
    let cas = authority.cas_store()?;
    let object = cas
        .get_object(object_hash)?
        .ok_or_else(|| anyhow::anyhow!("project_file object {object_hash} not found"))?;
    let file = ProjectFile::from_value(&object)?;
    let size =
        cas.materialize_blob_to_new_file(&file.blob_hash, target_path, file.normalized_mode)?;
    if size != file.size {
        let _ = fs::remove_file(target_path);
        anyhow::bail!(
            "project_file {} declared size {}, materialized {}",
            object_hash,
            file.size,
            size
        );
    }
    Ok(())
}
