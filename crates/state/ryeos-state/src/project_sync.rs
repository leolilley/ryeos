//! Project AI sync scope and manifest path validation.

use std::path::Component;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::objects::SourceManifest;

/// Scope declared by a project snapshot.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSyncScope {
    /// Project `.ai` allow-list only, safe for live remote AI deployment.
    /// Default: most local projects also contain large codebases or
    /// asset trees that should never be shipped to a remote node, so
    /// we sync only the curated `.ai/` subtree unless the operator
    /// explicitly opts into `full_project`.
    #[default]
    AiOnly,
    /// Full project snapshot used by existing push/execute flows.
    FullProject,
}

/// Managed project `.ai` roots that AI-only project sync may deploy.
///
/// This intentionally excludes `.ai/node/routes` and `.ai/services`; v1
/// project AI sync does not mutate the remote node's public HTTP surface.
pub const PROJECT_AI_SYNC_DIRS: &[&str] = &[
    ".ai/directives",
    ".ai/tools",
    ".ai/knowledge",
    ".ai/parsers",
    ".ai/handlers",
    ".ai/protocols",
    ".ai/node/engine/kinds",
    ".ai/node/verbs",
    ".ai/config/agent",
    ".ai/config/keys/trusted",
];

/// Validate all paths in a project manifest for the declared sync scope.
pub fn validate_project_manifest_paths(
    manifest: &SourceManifest,
    scope: ProjectSyncScope,
) -> Result<()> {
    for rel_path in manifest.item_source_hashes.keys() {
        validate_project_manifest_path(rel_path, scope)?;
    }
    Ok(())
}

/// Validate a single manifest path for the declared sync scope.
pub fn validate_project_manifest_path(rel_path: &str, scope: ProjectSyncScope) -> Result<()> {
    validate_safe_relative_path(rel_path)?;

    if scope == ProjectSyncScope::AiOnly {
        if is_project_ai_sync_root(rel_path) {
            anyhow::bail!(
                "AI-only project manifest path '{}' names a managed .ai sync root; expected a file below a managed root",
                rel_path
            );
        }
        if !is_project_ai_sync_path(rel_path) {
            anyhow::bail!(
                "AI-only project manifest path '{}' is outside managed .ai sync roots",
                rel_path
            );
        }
    }

    Ok(())
}

/// True when a relative path is inside one of the managed AI sync roots.
pub fn is_project_ai_sync_path(rel_path: &str) -> bool {
    PROJECT_AI_SYNC_DIRS
        .iter()
        .any(|root| rel_path == *root || rel_path.starts_with(&format!("{root}/")))
}

/// True when a relative path is exactly one of the managed AI sync roots.
pub fn is_project_ai_sync_root(rel_path: &str) -> bool {
    PROJECT_AI_SYNC_DIRS.iter().any(|root| rel_path == *root)
}

/// Basic relative path safety shared by full-project and AI-only snapshots.
pub fn validate_safe_relative_path(rel_path: &str) -> Result<()> {
    if rel_path.is_empty() {
        anyhow::bail!("manifest path must not be empty");
    }
    if rel_path.contains('\\') {
        anyhow::bail!("manifest path '{}' contains a backslash", rel_path);
    }

    let path = std::path::Path::new(rel_path);
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::ParentDir => anyhow::bail!("manifest path '{}' contains '..'", rel_path),
            Component::CurDir => anyhow::bail!("manifest path '{}' contains '.'", rel_path),
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("manifest path '{}' is absolute", rel_path)
            }
        }
    }

    // `Path::components` can normalize some odd forms; reject strings
    // that did not yield any normal component as a final guard.
    if !path.components().any(|c| matches!(c, Component::Normal(_))) {
        return Err(anyhow!(
            "manifest path '{}' has no normal components",
            rel_path
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn manifest(paths: &[&str]) -> SourceManifest {
        SourceManifest {
            item_source_hashes: paths
                .iter()
                .map(|p| ((*p).to_string(), "ab".repeat(32)))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn ai_only_accepts_managed_roots() {
        let m = manifest(&[".ai/directives/foo.md", ".ai/tools/app/tool.yaml"]);
        validate_project_manifest_paths(&m, ProjectSyncScope::AiOnly).unwrap();
    }

    #[test]
    fn ai_only_rejects_exact_managed_roots() {
        for path in [".ai/directives", ".ai/tools", ".ai/config/keys/trusted"] {
            let err = validate_project_manifest_paths(&manifest(&[path]), ProjectSyncScope::AiOnly)
                .expect_err("exact managed root path must be rejected");
            assert!(format!("{err:#}").contains("names a managed"));
        }
    }

    #[test]
    fn ai_only_rejects_app_code_and_unmanaged_ai() {
        for path in [
            "src/index.ts",
            ".ai/state/runtime.sqlite3",
            ".ai/config/keys/signing/private.pem",
            ".ai/node/routes/apply.yaml",
            ".ai/services/project/apply.yaml",
        ] {
            let err = validate_project_manifest_paths(&manifest(&[path]), ProjectSyncScope::AiOnly)
                .expect_err("path must be rejected");
            assert!(format!("{err:#}").contains("outside managed"));
        }
    }

    #[test]
    fn rejects_unsafe_paths_for_all_scopes() {
        for path in ["../escape", "/absolute/path", "./dot", "a/../b", "a\\b"] {
            validate_project_manifest_paths(&manifest(&[path]), ProjectSyncScope::FullProject)
                .expect_err("unsafe full-project path must be rejected");
            validate_project_manifest_paths(&manifest(&[path]), ProjectSyncScope::AiOnly)
                .expect_err("unsafe ai-only path must be rejected");
        }
    }

    #[test]
    fn ai_only_does_not_prefix_match_spoofed_roots() {
        let m = manifest(&[".ai/directives-link/evil.md"]);
        validate_project_manifest_paths(&m, ProjectSyncScope::AiOnly)
            .expect_err("prefix spoof must be rejected");
    }
}
