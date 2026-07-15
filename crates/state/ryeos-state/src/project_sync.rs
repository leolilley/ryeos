//! Project AI sync scope and manifest path validation.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::ignore::IgnoreMatcher;
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
    surface(".ai/graphs", ProjectAiSurfaceKind::ProjectItems, true),
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
        ".ai/config/execution",
        ProjectAiSurfaceKind::ProjectConfig,
        true,
    ),
    surface(
        ".ai/config/directive-runtime",
        ProjectAiSurfaceKind::ProjectConfig,
        true,
    ),
    surface(
        ".ai/config/ryeos-runtime",
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

/// Secrets that must never leave the machine. **Code-enforced floor**: this is
/// enforced for every sync scope, and configuration may only ever *add* to it,
/// never remove an entry. Shipping any of these would leak a credential.
pub const NEVER_DEPLOY_SECRETS: &[&str] = &[
    ".ai/node/identity",
    ".ai/node/auth",
    ".ai/node/vault",
    ".ai/config/keys/signing",
];

/// Node-owned runtime state that belongs to whichever node runs it (not project
/// content). **Code-enforced floor**: enforced for every sync scope; config may
/// only *add*. Deploying these would clobber or leak the remote's own state.
pub const NODE_OWNED: &[&str] = &[
    ".ai/state",
    ".ai/node/schedules",
    ".ai/node/routes",
    ".ai/node/bundles",
];

/// Match `rel_path` against a set of `.ai` prefixes on segment boundaries,
/// returning the matched prefix. `foo` matches `foo` and `foo/bar`, never
/// `foobar`.
fn matched_prefix(rel_path: &str, prefixes: &[&'static str]) -> Option<&'static str> {
    prefixes
        .iter()
        .copied()
        .find(|p| rel_path == *p || rel_path.starts_with(&format!("{p}/")))
}

fn surface_kind_str(kind: ProjectAiSurfaceKind) -> &'static str {
    match kind {
        ProjectAiSurfaceKind::ProjectItems => "project_items",
        ProjectAiSurfaceKind::ProjectConfig => "project_config",
        ProjectAiSurfaceKind::TrustPins => "trust_pins",
        ProjectAiSurfaceKind::ScheduleDeclarations => "schedule_declarations",
        ProjectAiSurfaceKind::NodeExtensionDeclarations => "node_extension_declarations",
    }
}

/// Render a read-only, human/LLM-readable view of the effective sync policy:
/// the deployable surfaces and the two code-enforced floors, plus a pointer to
/// the one editable input (`ignore_source`). This is a **discovery aid only** —
/// editing the generated file changes nothing; floors and surfaces are enforced
/// in code, and ignore is controlled by `ignore_source`.
pub fn render_effective_sync_policy_yaml(ignore_source: &str) -> String {
    let mut out = String::new();
    out.push_str("# GENERATED — read-only view of this node's effective sync policy.\n");
    out.push_str("# Editing this file does NOTHING. To change what is ignored, edit the file\n");
    out.push_str(&format!(
        "# named in `ignore_source` below ({ignore_source}).\n"
    ));
    out.push_str("# Secrets, node-owned state, and deployable surfaces are enforced in code\n");
    out.push_str("# (protocol v1) and cannot be loosened here.\n");
    out.push_str("version: 1\n");
    out.push_str(&format!("ignore_source: {ignore_source:?}\n"));
    out.push_str("# Credentials — never leave this machine, any sync scope.\n");
    out.push_str("never_deploy_secrets:\n");
    for p in NEVER_DEPLOY_SECRETS {
        out.push_str(&format!("  - {p:?}\n"));
    }
    out.push_str("# Node-owned runtime state — never deployed from a project.\n");
    out.push_str("node_owned:\n");
    for p in NODE_OWNED {
        out.push_str(&format!("  - {p:?}\n"));
    }
    out.push_str("# Deployable project content (AI-only sync).\n");
    out.push_str("deployable_surfaces:\n");
    for s in PROJECT_AI_SURFACES {
        out.push_str(&format!(
            "  - {{ root: {:?}, kind: {}, materialize_to_project: {} }}\n",
            s.root,
            surface_kind_str(s.kind),
            s.materialize_to_project
        ));
    }
    out
}

/// Classification of a relative path in a project AI snapshot.
///
/// Order of the variants mirrors the resolution precedence used by
/// [`classify_project_ai_path`]: `never_deploy_secrets` → `ignore` →
/// `node_owned` → `deployable` → fall-through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectAiPathClass {
    /// Under a [`NEVER_DEPLOY_SECRETS`] prefix — credential, never ships.
    NeverDeploySecret { prefix: &'static str },
    /// Matched the ignore matcher (junk / environment-specific).
    Ignored,
    /// Under a [`NODE_OWNED`] prefix — node runtime state, never ships.
    NodeOwned { prefix: &'static str },
    /// A deployable project surface.
    Deployable(ProjectAiSurface),
    /// Under `.ai/` but not a deployable surface.
    UnknownAiPath,
    /// Not `.ai/` content at all.
    NonAiPath,
}

/// Validate all paths in a project manifest for the declared sync scope.
///
/// `ignore`, when supplied, lets the validator treat ignore-matched paths as
/// non-deployable (e.g. junk that slipped into a manifest). The secret and
/// node-owned floors are enforced for **every** scope regardless of `ignore`.
pub fn validate_project_manifest_paths(
    manifest: &SourceManifest,
    scope: ProjectSyncScope,
    ignore: Option<&IgnoreMatcher>,
) -> Result<()> {
    for rel_path in manifest.item_source_hashes.keys() {
        validate_project_manifest_path(rel_path, scope, ignore)?;
    }
    Ok(())
}

/// Validate a single manifest path for the declared sync scope.
pub fn validate_project_manifest_path(
    rel_path: &str,
    scope: ProjectSyncScope,
    ignore: Option<&IgnoreMatcher>,
) -> Result<()> {
    validate_safe_relative_path(rel_path)?;

    // Scope-independent floor: secrets and node-owned runtime state must never
    // deploy, for AI-only AND full-project sync. This runs before any
    // scope-specific logic so the full-project path can't bypass it.
    if let Some(prefix) = matched_prefix(rel_path, NEVER_DEPLOY_SECRETS) {
        anyhow::bail!(
            "manifest path '{}' is under never-deploy secret prefix '{}' and must never leave this machine",
            rel_path,
            prefix
        );
    }
    if let Some(prefix) = matched_prefix(rel_path, NODE_OWNED) {
        anyhow::bail!(
            "manifest path '{}' is under node-owned runtime prefix '{}' and cannot be deployed",
            rel_path,
            prefix
        );
    }

    if scope == ProjectSyncScope::AiOnly {
        match classify_project_ai_path(rel_path, ignore) {
            ProjectAiPathClass::Deployable(surface) if rel_path == surface.root => {
                anyhow::bail!(
                    "AI-only project manifest path '{}' names a deployable .ai surface root; expected a file below the surface root",
                    rel_path
                );
            }
            ProjectAiPathClass::Deployable(_) => {}
            // Floors already bailed above; defensive, keeps messages consistent.
            ProjectAiPathClass::NeverDeploySecret { prefix } => {
                anyhow::bail!(
                    "AI-only project manifest path '{}' is under never-deploy secret prefix '{}'",
                    rel_path,
                    prefix
                );
            }
            ProjectAiPathClass::NodeOwned { prefix } => {
                anyhow::bail!(
                    "AI-only project manifest path '{}' is under node-owned runtime prefix '{}' and cannot be deployed",
                    rel_path,
                    prefix
                );
            }
            ProjectAiPathClass::Ignored => {
                anyhow::bail!(
                    "AI-only project manifest path '{}' matches an ignore pattern and must not be in the manifest",
                    rel_path
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
///
/// Resolution precedence: `never_deploy_secrets` → `ignore` → `node_owned` →
/// `deployable` → fall-through. `ignore` is checked **before** deployable so a
/// junk file inside a deployable surface (e.g. `.ai/tools/x/__pycache__/y.pyc`)
/// classifies as `Ignored`, never `Deployable`.
pub fn classify_project_ai_path(
    rel_path: &str,
    ignore: Option<&IgnoreMatcher>,
) -> ProjectAiPathClass {
    if let Some(prefix) = matched_prefix(rel_path, NEVER_DEPLOY_SECRETS) {
        return ProjectAiPathClass::NeverDeploySecret { prefix };
    }

    if let Some(m) = ignore {
        if m.is_ignored(rel_path) {
            return ProjectAiPathClass::Ignored;
        }
    }

    if let Some(prefix) = matched_prefix(rel_path, NODE_OWNED) {
        return ProjectAiPathClass::NodeOwned { prefix };
    }

    for surface in PROJECT_AI_SURFACES {
        if rel_path == surface.root || rel_path.starts_with(&format!("{}/", surface.root)) {
            return ProjectAiPathClass::Deployable(*surface);
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
    crate::objects::validate_canonical_project_relative_path(rel_path)
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
            ".ai/graphs/app/flow.yaml",
            ".ai/config/execution/execution.yaml",
            ".ai/config/directive-runtime/limits.yaml",
            ".ai/config/schedules/snap-track.yaml",
        ]);
        validate_project_manifest_paths(&m, ProjectSyncScope::AiOnly, None).unwrap();
    }

    #[test]
    fn ai_only_rejects_exact_managed_roots() {
        for path in [
            ".ai/directives",
            ".ai/tools",
            ".ai/graphs",
            ".ai/config/execution",
            ".ai/config/ryeos-runtime",
            ".ai/config/keys/trusted",
            ".ai/config/schedules",
        ] {
            let err =
                validate_project_manifest_paths(&manifest(&[path]), ProjectSyncScope::AiOnly, None)
                    .expect_err("exact managed root path must be rejected");
            assert!(format!("{err:#}").contains("names a deployable"));
        }
    }

    #[test]
    fn ai_only_rejects_app_code_and_unmanaged_ai() {
        for (path, expected) in [
            ("src/index.ts", "non-.ai project content"),
            (".ai/state/runtime.sqlite3", "node-owned runtime prefix"),
            (
                ".ai/config/keys/signing/private.pem",
                "never-deploy secret prefix",
            ),
            (".ai/node/routes/apply.yaml", "node-owned runtime prefix"),
            (
                ".ai/services/project/apply.yaml",
                "outside deployable project surfaces",
            ),
        ] {
            let err =
                validate_project_manifest_paths(&manifest(&[path]), ProjectSyncScope::AiOnly, None)
                    .expect_err("path must be rejected");
            assert!(
                format!("{err:#}").contains(expected),
                "error for {path} should contain {expected:?}, got {err:#}"
            );
        }
    }

    #[test]
    fn floors_enforced_for_full_project_too() {
        // Secrets and node-owned runtime state must be rejected even under
        // full_project sync, which previously only checked path safety.
        for path in [
            ".ai/node/identity/private_key.pem",
            ".ai/config/keys/signing/k.pem",
            ".ai/state/runtime.sqlite3",
            ".ai/node/routes/apply.yaml",
        ] {
            validate_project_manifest_paths(
                &manifest(&[path]),
                ProjectSyncScope::FullProject,
                None,
            )
            .expect_err("secret/node-owned path must be rejected for full_project");
        }
    }

    #[test]
    fn ignored_file_inside_surface_is_not_deployable() {
        let ignore = crate::ignore::matcher_from_builtins();
        // A junk file inside a deployable surface classifies as Ignored, not
        // Deployable — ignore wins over the surface allowlist.
        assert_eq!(
            classify_project_ai_path(".ai/tools/app/__pycache__/x.pyc", Some(&ignore)),
            ProjectAiPathClass::Ignored
        );
    }

    #[test]
    fn classifies_project_ai_paths() {
        assert!(matches!(
            classify_project_ai_path(".ai/config/schedules/snap-track.yaml", None),
            ProjectAiPathClass::Deployable(ProjectAiSurface {
                kind: ProjectAiSurfaceKind::ScheduleDeclarations,
                ..
            })
        ));
        assert!(matches!(
            classify_project_ai_path(".ai/graphs/snap-track/show_rescrape.yaml", None),
            ProjectAiPathClass::Deployable(ProjectAiSurface {
                kind: ProjectAiSurfaceKind::ProjectItems,
                ..
            })
        ));
        assert!(matches!(
            classify_project_ai_path(".ai/config/execution/execution.yaml", None),
            ProjectAiPathClass::Deployable(ProjectAiSurface {
                kind: ProjectAiSurfaceKind::ProjectConfig,
                ..
            })
        ));
        assert!(matches!(
            classify_project_ai_path(".ai/node/schedules/foo.yaml", None),
            ProjectAiPathClass::NodeOwned {
                prefix: ".ai/node/schedules"
            }
        ));
        assert!(matches!(
            classify_project_ai_path(".ai/node/identity/key.pem", None),
            ProjectAiPathClass::NeverDeploySecret {
                prefix: ".ai/node/identity"
            }
        ));
        assert_eq!(
            classify_project_ai_path(".ai/unknown/foo.yaml", None),
            ProjectAiPathClass::UnknownAiPath
        );
        assert_eq!(
            classify_project_ai_path("src/index.ts", None),
            ProjectAiPathClass::NonAiPath
        );
    }

    #[test]
    fn rejects_unsafe_paths_for_all_scopes() {
        for path in ["../escape", "/absolute/path", "./dot", "a/../b", "a\\b"] {
            validate_project_manifest_paths(
                &manifest(&[path]),
                ProjectSyncScope::FullProject,
                None,
            )
            .expect_err("unsafe full-project path must be rejected");
            validate_project_manifest_paths(&manifest(&[path]), ProjectSyncScope::AiOnly, None)
                .expect_err("unsafe ai-only path must be rejected");
        }
    }

    #[test]
    fn ai_only_does_not_prefix_match_spoofed_roots() {
        let m = manifest(&[".ai/directives-link/evil.md"]);
        validate_project_manifest_paths(&m, ProjectSyncScope::AiOnly, None)
            .expect_err("prefix spoof must be rejected");
    }

    #[test]
    fn rendered_policy_is_valid_yaml_with_all_buckets() {
        let yaml = render_effective_sync_policy_yaml(".ai/node/ingest/ignore.yaml");
        let v: serde_yaml::Value =
            serde_yaml::from_str(&yaml).expect("generated policy is valid YAML");
        assert_eq!(v["version"].as_u64(), Some(1));
        assert_eq!(
            v["ignore_source"].as_str(),
            Some(".ai/node/ingest/ignore.yaml")
        );
        let secrets = v["never_deploy_secrets"].as_sequence().unwrap();
        assert!(secrets
            .iter()
            .any(|x| x.as_str() == Some(".ai/node/identity")));
        let node_owned = v["node_owned"].as_sequence().unwrap();
        assert!(node_owned.iter().any(|x| x.as_str() == Some(".ai/state")));
        assert_eq!(
            v["deployable_surfaces"].as_sequence().unwrap().len(),
            PROJECT_AI_SURFACES.len()
        );
    }
}
