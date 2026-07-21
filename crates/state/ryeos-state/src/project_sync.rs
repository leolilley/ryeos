//! Project AI sync scope and manifest path validation.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::ignore::IgnoreMatcher;
use crate::objects::{ProjectSnapshotPolicy, ProjectTree, SourceManifest};

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

pub const PROJECT_SNAPSHOT_CONFIG_RELATIVE: &str = ".ai/config/execution/project-snapshot.yaml";

/// Content identity used when a project deliberately has no authored snapshot
/// policy. Absence is part of the captured policy, not an omitted fact.
pub fn absent_project_snapshot_config_hash() -> String {
    hex_sha256(br#"{"state":"absent"}"#)
}

/// Complete source identity for a synthetic capture that deliberately has no
/// authored project policy. Synthetic/no-project snapshots still bind both
/// policy inputs; absence must never be represented by an omitted map entry.
pub fn absent_project_snapshot_source_hashes(
    node_matcher: &IgnoreMatcher,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut source_hashes = std::collections::BTreeMap::new();
    source_hashes.insert(
        "project_config".to_string(),
        absent_project_snapshot_config_hash(),
    );
    let node_patterns = node_matcher.canonical_patterns().to_vec();
    let node_identity = lillux::canonical_json(&serde_json::json!({
        "schema": 1,
        "patterns": node_patterns,
    }))?;
    source_hashes.insert(
        "node_additions".to_string(),
        hex_sha256(node_identity.as_bytes()),
    );
    Ok(source_hashes)
}

/// Bind a captured tree to the policy-source presence/content fact recorded in
/// its immutable policy. This treats absence as an explicit identity and
/// closes both missing→created and created→missing capture races.
pub fn validate_captured_policy_source(
    cas: &lillux::CasStore,
    tree: &ProjectTree,
    policy: &ProjectSnapshotPolicy,
) -> Result<()> {
    let expected = policy
        .source_hashes
        .get("project_config")
        .ok_or_else(|| anyhow::anyhow!("project snapshot policy omitted project-config state"))?;
    let captured = tree.files.get(PROJECT_SNAPSHOT_CONFIG_RELATIVE);
    if expected == &absent_project_snapshot_config_hash() {
        anyhow::ensure!(
            captured.is_none(),
            "project snapshot policy source appeared during project capture"
        );
        return Ok(());
    }
    let object_hash = captured.ok_or_else(|| {
        anyhow::anyhow!("project snapshot policy source disappeared during capture")
    })?;
    let object = cas
        .get_object(object_hash)?
        .ok_or_else(|| anyhow::anyhow!("captured policy ProjectFile is absent"))?;
    let file = crate::objects::ProjectFile::from_value(&object)?;
    anyhow::ensure!(
        &file.blob_hash == expected,
        "project snapshot policy changed during capture (policy={}, tree={})",
        expected,
        file.blob_hash
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSnapshotConfig {
    pub schema: u32,
    #[serde(default)]
    pub exclusions: Vec<String>,
}

impl ProjectSnapshotConfig {
    pub const SCHEMA: u32 = 1;

    fn empty() -> Self {
        Self {
            schema: Self::SCHEMA,
            exclusions: Vec::new(),
        }
    }
}

/// Build the immutable policy for one capture. The committed project source is
/// optional; the node matcher remains additive and cannot loosen code floors.
pub fn capture_snapshot_policy(
    project_root: &std::path::Path,
    node_matcher: &IgnoreMatcher,
    scope: ProjectSyncScope,
) -> Result<ProjectSnapshotPolicy> {
    let pinned = lillux::PinnedDirectory::open(project_root)?.ok_or_else(|| {
        anyhow::anyhow!("project root does not exist: {}", project_root.display())
    })?;
    let policy = capture_snapshot_policy_from_pinned(&pinned, node_matcher, scope)?;
    pinned.ensure_path_binding()?;
    Ok(policy)
}

pub fn capture_snapshot_policy_from_pinned(
    project_root: &lillux::PinnedDirectory,
    node_matcher: &IgnoreMatcher,
    scope: ProjectSyncScope,
) -> Result<ProjectSnapshotPolicy> {
    let config_file = open_optional_pinned_relative(
        project_root,
        std::path::Path::new(PROJECT_SNAPSHOT_CONFIG_RELATIVE),
    )?;
    let (config, project_source_hash) = match config_file {
        Some(mut file) => {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut bytes)?;
            let config: ProjectSnapshotConfig = serde_yaml::from_slice(&bytes)
                .map_err(|error| anyhow::anyhow!("invalid project snapshot policy: {error}"))?;
            anyhow::ensure!(
                config.schema == ProjectSnapshotConfig::SCHEMA,
                "project snapshot policy schema mismatch: expected {}, got {}",
                ProjectSnapshotConfig::SCHEMA,
                config.schema
            );
            (config, Some(hex_sha256(&bytes)))
        }
        None => (ProjectSnapshotConfig::empty(), None),
    };

    let mut source_hashes = absent_project_snapshot_source_hashes(node_matcher)?;
    if let Some(project_source_hash) = project_source_hash {
        source_hashes.insert("project_config".to_string(), project_source_hash);
    }

    ProjectSnapshotPolicy::new(
        scope,
        config.exclusions,
        node_matcher.canonical_patterns().to_vec(),
        source_hashes,
    )
}

fn open_optional_pinned_relative(
    root: &lillux::PinnedDirectory,
    relative: &std::path::Path,
) -> Result<Option<std::fs::File>> {
    use std::path::Component;

    let mut directory = root.try_clone()?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            anyhow::bail!("pinned policy path is not normalized");
        };
        if components.peek().is_none() {
            return directory.open_regular(name, false);
        }
        let Some(child) = directory.open_child_directory(name)? else {
            return Ok(None);
        };
        directory = child;
    }
    Ok(None)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
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
    ".env",
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

/// Central, non-bypassable live-execution control-plane floor. The same
/// classification owns snapshot/deploy safety; live execution serializes this
/// exact policy into its authority rather than duplicating path literals in an
/// executor or adapter.
pub fn live_execution_denied_control_paths() -> Vec<String> {
    let mut paths = NEVER_DEPLOY_SECRETS
        .iter()
        .chain(NODE_OWNED.iter())
        .map(|path| (*path).to_string())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

/// Canonical identity of the non-bypassable project snapshot floor. Category
/// prefixes keep otherwise identical path strings semantically distinct.
pub fn snapshot_floor_rules() -> Vec<String> {
    let mut rules = NEVER_DEPLOY_SECRETS
        .iter()
        .map(|path| format!("never_deploy_secret:{path}"))
        .chain(NODE_OWNED.iter().map(|path| format!("node_owned:{path}")))
        .chain([
            "transaction_artifact:.ryeos-pull-staging-*".to_string(),
            "transaction_artifact:.ryeos-pull-backup-*".to_string(),
            "transaction_artifact:.ryeos-quarantine.*".to_string(),
            "transaction_artifact:.ryeos-pull.lock".to_string(),
        ])
        .collect::<Vec<_>>();
    rules.sort();
    rules
}

/// Match `rel_path` against a set of `.ai` prefixes on segment boundaries,
/// returning the matched prefix. `foo` matches `foo` and `foo/bar`, never
/// `foobar`.
fn matched_prefix(rel_path: &str, prefixes: &[&'static str]) -> Option<&'static str> {
    prefixes
        .iter()
        .copied()
        .find(|p| rel_path == *p || rel_path.starts_with(&format!("{p}/")))
}

pub fn is_project_snapshot_floor_excluded(rel_path: &str) -> bool {
    let transaction_artifact = rel_path.split('/').any(|component| {
        component.starts_with(".ryeos-pull-staging-")
            || component.starts_with(".ryeos-pull-backup-")
            || component.starts_with(".ryeos-quarantine.")
            || component == ".ryeos-pull.lock"
    });
    matched_prefix(rel_path, NEVER_DEPLOY_SECRETS).is_some()
        || matched_prefix(rel_path, NODE_OWNED).is_some()
        || transaction_artifact
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

/// Validate a complete project tree against the exact immutable policy stored
/// beside its snapshot. Current node ignore configuration is intentionally not
/// consulted here.
pub fn validate_project_tree_paths(
    tree: &ProjectTree,
    policy: &ProjectSnapshotPolicy,
) -> Result<()> {
    policy.validate()?;
    let matcher = policy.matcher()?;
    for rel_path in tree.files.keys() {
        validate_project_manifest_path(rel_path, policy.sync_scope, Some(&matcher))?;
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
    if is_project_snapshot_floor_excluded(rel_path) {
        anyhow::bail!(
            "manifest path '{}' is a RyeOS transaction artifact and cannot be deployed",
            rel_path
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
            ".env",
            ".ai/node/identity/private_key.pem",
            ".ai/config/keys/signing/k.pem",
            ".ai/state/runtime.sqlite3",
            ".ai/node/routes/apply.yaml",
            "src/.ryeos-quarantine.1.2",
            "nested/.ryeos-pull-backup-1.2/journal.json",
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
            classify_project_ai_path(".env", None),
            ProjectAiPathClass::NeverDeploySecret { prefix: ".env" }
        ));
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
        assert!(secrets.iter().any(|x| x.as_str() == Some(".env")));
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
