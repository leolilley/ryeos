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
    ingest_project_tree_with_operational_exclusions(authority, guard, project_root, policy, &[])
}

/// Capture a daemon-owned execution workspace while omitting operational
/// shadow roots that were populated from separately admitted realizations.
/// The exclusions are not author policy and never change source capture;
/// they prevent mounted/copy-bound inputs from becoming project outputs.
pub fn ingest_project_tree_with_operational_exclusions(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    project_root: &lillux::PinnedDirectory,
    policy: &ProjectSnapshotPolicy,
    operational_exclusions: &[String],
) -> Result<ProjectTree> {
    authority.ensure_guard(guard)?;
    policy.validate()?;
    let mut previous: Option<&str> = None;
    for exclusion in operational_exclusions {
        ryeos_state::project_sync::validate_safe_relative_path(exclusion)?;
        if previous.is_some_and(|value| value >= exclusion.as_str()) {
            anyhow::bail!("operational project exclusions are not uniquely path-sorted");
        }
        previous = Some(exclusion);
    }
    let matcher = policy.matcher()?;
    let cas = authority.cas_store()?;
    let mut files = std::collections::BTreeMap::new();
    let mut descriptor_bytes = 0_u64;
    project_root.visit_regular_files_bounded(
        lillux::DirectoryTraversalBudget::new(
            ryeos_state::project_sync::MAX_PROJECT_TREE_ENTRIES,
            ryeos_state::project_sync::MAX_PROJECT_TREE_DEPTH,
        ),
        |relative, is_directory| {
            let rel = canonical_relative_path(relative)?;
            if is_operationally_excluded(&rel, operational_exclusions)
                || ryeos_state::project_sync::is_project_snapshot_floor_excluded(&rel)
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
            if files.len() >= ryeos_state::project_sync::MAX_PROJECT_TREE_FILES {
                anyhow::bail!(
                    "project capture exceeds {} regular files",
                    ryeos_state::project_sync::MAX_PROJECT_TREE_FILES
                );
            }
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
            let object_bytes = lillux::canonical_json(&project_file.to_value())?.len() as u64;
            descriptor_bytes = descriptor_bytes
                .checked_add(object_bytes)
                .and_then(|total| total.checked_add(rel.len() as u64))
                .ok_or_else(|| anyhow::anyhow!("project capture descriptor byte count overflow"))?;
            if descriptor_bytes
                > ryeos_state::project_materialization::MAX_PROJECT_TREE_DESCRIPTOR_BYTES
            {
                anyhow::bail!(
                    "project capture exceeds {} descriptor bytes",
                    ryeos_state::project_materialization::MAX_PROJECT_TREE_DESCRIPTOR_BYTES
                );
            }
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

fn is_operationally_excluded(path: &str, exclusions: &[String]) -> bool {
    exclusions.iter().any(|root| {
        path == root
            || path
                .strip_prefix(root)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
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
    let file = ryeos_state::project_materialization::load_project_file_bounded(&cas, object_hash)?
        .ok_or_else(|| anyhow::anyhow!("project_file object {object_hash} not found"))?;
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

#[cfg(test)]
mod tests {
    use super::is_operationally_excluded;

    #[test]
    fn operational_shadow_roots_match_only_segment_bounded_descendants() {
        let exclusions = vec!["vendor/runtime".to_string()];
        assert!(is_operationally_excluded("vendor/runtime", &exclusions));
        assert!(is_operationally_excluded(
            "vendor/runtime/lib/module.py",
            &exclusions
        ));
        assert!(!is_operationally_excluded(
            "vendor/runtime-extra/module.py",
            &exclusions
        ));
        assert!(!is_operationally_excluded("vendor", &exclusions));
    }
}
