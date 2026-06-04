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

/// Kind of deployable project `.ai` surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectAiSurfaceKind {
    /// Signed RyeOS items that materialize as project content.
    ProjectItems,
    /// Project-authored configuration that materializes as project intent.
    ProjectConfig,
    /// Project-authored trust pins.
    TrustPins,
    /// Project-authored schedule declarations; reconciled separately into
    /// node-owned scheduler runtime specs.
    ScheduleDeclarations,
    /// Project/bundle-authored node extension declarations, not runtime state.
    NodeExtensionDeclarations,
}

/// Deployable project `.ai` surface descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectAiSurface {
    pub root: &'static str,
    pub kind: ProjectAiSurfaceKind,
    pub materialize_to_project: bool,
}

const fn surface(
    root: &'static str,
    kind: ProjectAiSurfaceKind,
    materialize_to_project: bool,
) -> ProjectAiSurface {
    ProjectAiSurface {
        root,
        kind,
        materialize_to_project,
    }
}

/// Deployable project `.ai` surfaces that AI-only project sync may ingest.
///
/// This intentionally excludes node-owned runtime state such as
/// `.ai/node/routes`, `.ai/node/schedules`, `.ai/state`, and signing keys.
pub const PROJECT_AI_SURFACES: &[ProjectAiSurface] = &[
    surface(".ai/directives", ProjectAiSurfaceKind::ProjectItems, true),
    surface(".ai/tools", ProjectAiSurfaceKind::ProjectItems, true),
    surface(".ai/knowledge", ProjectAiSurfaceKind::ProjectItems, true),
    surface(".ai/parsers", ProjectAiSurfaceKind::ProjectItems, true),
    surface(".ai/handlers", ProjectAiSurfaceKind::ProjectItems, true),
    surface(".ai/protocols", ProjectAiSurfaceKind::ProjectItems, true),
    surface(
        ".ai/node/engine/kinds",
        ProjectAiSurfaceKind::NodeExtensionDeclarations,
        true,
    ),
    surface(
        ".ai/node/commands",
        ProjectAiSurfaceKind::NodeExtensionDeclarations,
        true,
    ),
    surface(
        ".ai/config/agent",
        ProjectAiSurfaceKind::ProjectConfig,
        true,
    ),
    surface(
        ".ai/config/keys/trusted",
        ProjectAiSurfaceKind::TrustPins,
        true,
    ),
    surface(
        ".ai/config/schedules",
        ProjectAiSurfaceKind::ScheduleDeclarations,
        true,
    ),
];

/// Node-local or runtime-owned prefixes that project AI sync must not deploy.
pub const PROJECT_AI_LOCAL_ONLY_PREFIXES: &[&str] = &[
    ".ai/state",
    ".ai/node/schedules",
    ".ai/node/routes",
    ".ai/node/identity",
    ".ai/node/auth",
    ".ai/node/vault",
    ".ai/node/bundles",
    ".ai/config/keys/signing",
];

/// Classification of a relative path in a project AI snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectAiPathClass {
    Deployable(ProjectAiSurface),
    LocalOnly { prefix: &'static str },
    UnknownAiPath,
    NonAiPath,
}

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
        match classify_project_ai_path(rel_path) {
            ProjectAiPathClass::Deployable(surface) if rel_path == surface.root => {
                anyhow::bail!(
                    "AI-only project manifest path '{}' names a deployable .ai surface root; expected a file below the surface root",
                    rel_path
                );
            }
            ProjectAiPathClass::Deployable(_) => {}
            ProjectAiPathClass::LocalOnly { prefix } => {
                anyhow::bail!(
                    "AI-only project manifest path '{}' is under node-local/runtime-owned .ai prefix '{}' and cannot be deployed",
                    rel_path,
                    prefix
                );
            }
            ProjectAiPathClass::UnknownAiPath => {
                anyhow::bail!(
                    "AI-only project manifest path '{}' is under .ai but outside deployable project surfaces",
                    rel_path
                );
            }
            ProjectAiPathClass::NonAiPath => {
                anyhow::bail!(
                    "AI-only project manifest path '{}' is non-.ai project content; use full_project sync for app code",
                    rel_path
                );
            }
        }
    }

    Ok(())
}

/// Classify a relative path for AI-only project sync diagnostics.
pub fn classify_project_ai_path(rel_path: &str) -> ProjectAiPathClass {
    for surface in PROJECT_AI_SURFACES {
        if rel_path == surface.root || rel_path.starts_with(&format!("{}/", surface.root)) {
            return ProjectAiPathClass::Deployable(*surface);
        }
    }

    for prefix in PROJECT_AI_LOCAL_ONLY_PREFIXES {
        if rel_path == *prefix || rel_path.starts_with(&format!("{prefix}/")) {
            return ProjectAiPathClass::LocalOnly { prefix };
        }
    }

    if rel_path == ".ai" || rel_path.starts_with(".ai/") {
        ProjectAiPathClass::UnknownAiPath
    } else {
        ProjectAiPathClass::NonAiPath
    }
}

/// True when a relative path is inside one of the managed AI sync roots.
pub fn is_project_ai_sync_path(rel_path: &str) -> bool {
    PROJECT_AI_SURFACES.iter().any(|surface| {
        rel_path == surface.root || rel_path.starts_with(&format!("{}/", surface.root))
    })
}

/// True when a relative path is exactly one of the managed AI sync roots.
pub fn is_project_ai_sync_root(rel_path: &str) -> bool {
    PROJECT_AI_SURFACES
        .iter()
        .any(|surface| rel_path == surface.root)
}

/// Project AI surfaces materialized to the live project path during apply.
pub fn materialized_project_ai_surface_roots() -> impl Iterator<Item = &'static str> {
    PROJECT_AI_SURFACES
        .iter()
        .filter(|surface| surface.materialize_to_project)
        .map(|surface| surface.root)
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
        let m = manifest(&[
            ".ai/directives/foo.md",
            ".ai/tools/app/tool.yaml",
            ".ai/config/schedules/snap-track.yaml",
        ]);
        validate_project_manifest_paths(&m, ProjectSyncScope::AiOnly).unwrap();
    }

    #[test]
    fn ai_only_rejects_exact_managed_roots() {
        for path in [
            ".ai/directives",
            ".ai/tools",
            ".ai/config/keys/trusted",
            ".ai/config/schedules",
        ] {
            let err = validate_project_manifest_paths(&manifest(&[path]), ProjectSyncScope::AiOnly)
                .expect_err("exact managed root path must be rejected");
            assert!(format!("{err:#}").contains("names a deployable"));
        }
    }

    #[test]
    fn ai_only_rejects_app_code_and_unmanaged_ai() {
        for (path, expected) in [
            ("src/index.ts", "non-.ai project content"),
            (".ai/state/runtime.sqlite3", "node-local/runtime-owned"),
            (
                ".ai/config/keys/signing/private.pem",
                "node-local/runtime-owned",
            ),
            (".ai/node/routes/apply.yaml", "node-local/runtime-owned"),
            (
                ".ai/services/project/apply.yaml",
                "outside deployable project surfaces",
            ),
        ] {
            let err = validate_project_manifest_paths(&manifest(&[path]), ProjectSyncScope::AiOnly)
                .expect_err("path must be rejected");
            assert!(
                format!("{err:#}").contains(expected),
                "error for {path} should contain {expected:?}, got {err:#}"
            );
        }
    }

    #[test]
    fn classifies_project_ai_paths() {
        assert!(matches!(
            classify_project_ai_path(".ai/config/schedules/snap-track.yaml"),
            ProjectAiPathClass::Deployable(ProjectAiSurface {
                kind: ProjectAiSurfaceKind::ScheduleDeclarations,
                ..
            })
        ));
        assert!(matches!(
            classify_project_ai_path(".ai/node/schedules/foo.yaml"),
            ProjectAiPathClass::LocalOnly {
                prefix: ".ai/node/schedules"
            }
        ));
        assert_eq!(
            classify_project_ai_path(".ai/unknown/foo.yaml"),
            ProjectAiPathClass::UnknownAiPath
        );
        assert_eq!(
            classify_project_ai_path("src/index.ts"),
            ProjectAiPathClass::NonAiPath
        );
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
