//! Project AI sync scope and manifest path validation.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::ignore::IgnoreMatcher;
use crate::objects::{ProjectFile, ProjectSnapshotPolicy, ProjectTree, SourceManifest};

pub const PROJECT_CONFIG_POLICY_SOURCE: &str = "project_config";
pub const NODE_PATTERNS_POLICY_SOURCE: &str = "node_patterns";
const NODE_PATTERNS_PROJECTION_SCHEMA: u32 = 1;

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
pub const MAX_PROJECT_SNAPSHOT_CONFIG_BYTES: u64 = 1024 * 1024;
pub const MAX_PROJECT_TREE_FILES: usize = 100_000;
pub const MAX_PROJECT_TREE_ENTRIES: usize = MAX_PROJECT_TREE_FILES * 2;
pub const MAX_PROJECT_TREE_DEPTH: usize = 64;

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
        PROJECT_CONFIG_POLICY_SOURCE.to_string(),
        absent_project_snapshot_config_hash(),
    );
    source_hashes.insert(
        NODE_PATTERNS_POLICY_SOURCE.to_string(),
        project_snapshot_node_patterns_hash(node_matcher.canonical_patterns())?,
    );
    Ok(source_hashes)
}

/// Snapshot-owned semantic identity for the exact conventional patterns used
/// during capture. This deliberately does not reproduce the node-policy
/// section kind or schema; the node-policy compiler remains their sole owner.
pub fn project_snapshot_node_patterns_hash(patterns: &[String]) -> Result<String> {
    let identity = lillux::canonical_json(&serde_json::json!({
        "kind": "project_snapshot_node_patterns",
        "schema": NODE_PATTERNS_PROJECTION_SCHEMA,
        "patterns": patterns,
    }))?;
    Ok(hex_sha256(identity.as_bytes()))
}

/// Bind a captured tree to the policy-source presence/content fact recorded in
/// its immutable policy. This treats absence as an explicit identity and
/// closes both missing→created and created→missing capture races.
pub fn validate_captured_policy_source(
    cas: &lillux::CasStore,
    tree: &ProjectTree,
    policy: &ProjectSnapshotPolicy,
) -> Result<()> {
    let mut captured_files = std::collections::BTreeMap::new();
    if let Some(object_hash) = tree.files.get(PROJECT_SNAPSHOT_CONFIG_RELATIVE) {
        let file = crate::project_materialization::load_project_file_bounded(cas, object_hash)?
            .ok_or_else(|| anyhow::anyhow!("captured policy ProjectFile is absent"))?;
        captured_files.insert(PROJECT_SNAPSHOT_CONFIG_RELATIVE.to_owned(), file);
    }
    validate_captured_policy_source_from_files(tree, policy, &captured_files)
}

/// Validate the captured project-policy source against already-decoded,
/// bounded ProjectFile descriptors. Policy-source semantics live here; CAS
/// transport and descriptor bounds remain with project materialization.
pub fn validate_captured_policy_source_from_files(
    tree: &ProjectTree,
    policy: &ProjectSnapshotPolicy,
    captured_files: &std::collections::BTreeMap<String, ProjectFile>,
) -> Result<()> {
    let expected = policy
        .source_hashes
        .get(PROJECT_CONFIG_POLICY_SOURCE)
        .ok_or_else(|| anyhow::anyhow!("project snapshot policy omitted project-config state"))?;
    let captured = tree.files.get(PROJECT_SNAPSHOT_CONFIG_RELATIVE);
    if expected == &absent_project_snapshot_config_hash() {
        anyhow::ensure!(
            captured.is_none(),
            "project snapshot policy source appeared during project capture"
        );
        return Ok(());
    }
    captured.ok_or_else(|| {
        anyhow::anyhow!("project snapshot policy source disappeared during capture")
    })?;
    let file = captured_files
        .get(PROJECT_SNAPSHOT_CONFIG_RELATIVE)
        .ok_or_else(|| anyhow::anyhow!("captured policy ProjectFile is absent"))?;
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
/// optional; the node matcher is the exact signed conventional-ignore policy
/// and remains independent from the non-bypassable structural floor.
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
        Some(file) => {
            let bytes = file.read_bounded(MAX_PROJECT_SNAPSHOT_CONFIG_BYTES)?;
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
        source_hashes.insert(
            PROJECT_CONFIG_POLICY_SOURCE.to_string(),
            project_source_hash,
        );
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
) -> Result<Option<lillux::PinnedRegularFile>> {
    use std::path::Component;

    let mut directory = root.try_clone()?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            anyhow::bail!("pinned policy path is not normalized");
        };
        if components.peek().is_none() {
            return directory.open_pinned_regular(name, false);
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

/// Namespace shape owned by one project sync-include entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncIncludeShape {
    /// One exact regular file.
    File,
    /// A directory and the complete subtree below it.
    Directory,
}

/// One part of a project's `.ai` tree that project sync carries to another
/// node. A UI Surface (a `surface:` item) is unrelated to this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncIncludeEntry {
    pub root: String,
    pub shape: SyncIncludeShape,
    pub materialize_to_project: bool,
}

impl SyncIncludeEntry {
    pub fn directory(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            shape: SyncIncludeShape::Directory,
            materialize_to_project: true,
        }
    }

    pub fn file(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            shape: SyncIncludeShape::File,
            materialize_to_project: true,
        }
    }

    /// True when `rel_path` is this entry's file or lies inside its directory.
    pub fn matches_path(&self, rel_path: &str) -> bool {
        match self.shape {
            SyncIncludeShape::File => rel_path == self.root,
            SyncIncludeShape::Directory => {
                rel_path == self.root
                    || rel_path
                        .strip_prefix(self.root.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            }
        }
    }
}

/// True when `outer` equals `inner` or is a path-segment ancestor of it.
fn path_contains(outer: &str, inner: &str) -> bool {
    inner == outer
        || inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Roots a `sync_include` declaration may never touch, beyond the snapshot
/// floors: all signing and other key material under `.ai/config/keys`. (The
/// frozen snapshot floor names only `.ai/config/keys/signing`.)
pub const SYNC_INCLUDE_PROTECTED: &[&str] = &[".ai/config/keys"];

/// The only places under `.ai/node` a `sync_include` root may be. Everything
/// else there is node-owned: policies, native capacity, external execution
/// state, offline-reset and runtime-snapshot records, installed-bundle
/// registries, trust, identity.
pub const SYNC_INCLUDE_NODE_ALLOWED: &[&str] = &[".ai/node/engine/kinds", ".ai/node/commands"];

/// Why an AI-only path under `.ai` can never be project content, whatever any
/// table says: a live-execution-denied root, protected key material, or node
/// space outside the allowed declaration roots. Used on stored trees, where
/// table membership is not re-judged.
fn ai_only_protected_refusal(rel_path: &str) -> Option<String> {
    if let Some(prefix) = matched_prefix(rel_path, LIVE_EXECUTION_DENIED_ONLY) {
        return Some(format!("is under live-execution-denied path '{prefix}'"));
    }
    if let Some(prefix) = matched_prefix(rel_path, SYNC_INCLUDE_PROTECTED) {
        return Some(format!("is under protected key path '{prefix}'"));
    }
    let in_node = rel_path == ".ai/node" || rel_path.starts_with(".ai/node/");
    if in_node
        && !SYNC_INCLUDE_NODE_ALLOWED
            .iter()
            .any(|allowed| path_contains(allowed, rel_path))
    {
        return Some("is node-owned space under .ai/node".to_owned());
    }
    None
}

/// Refuse a declared root that is not a safe, canonical path below `.ai`, or
/// that equals, sits below, or sits above any floor path (secrets, node-owned
/// state) or live-execution-denied path.
pub fn validate_sync_include_entry(entry: &SyncIncludeEntry) -> Result<()> {
    let root = entry.root.as_str();
    validate_safe_relative_path(root).map_err(|error| {
        anyhow::anyhow!("sync_include root '{root}' is not a safe path: {error}")
    })?;
    if !root.starts_with(".ai/") {
        anyhow::bail!("sync_include root '{root}' must be a path below `.ai`");
    }
    for (label, paths) in [
        ("never-deploy secret", NEVER_DEPLOY_SECRETS),
        ("node-owned", NODE_OWNED),
        ("live-execution-denied", LIVE_EXECUTION_DENIED_ONLY),
        ("protected key", SYNC_INCLUDE_PROTECTED),
    ] {
        for protected in paths {
            if path_contains(protected, root) {
                anyhow::bail!(
                    "sync_include root '{root}' equals or lies below {label} path '{protected}'"
                );
            }
            if path_contains(root, protected) {
                anyhow::bail!("sync_include root '{root}' contains {label} path '{protected}'");
            }
        }
    }
    if (root == ".ai/node" || root.starts_with(".ai/node/"))
        && !SYNC_INCLUDE_NODE_ALLOWED
            .iter()
            .any(|allowed| path_contains(allowed, root))
    {
        anyhow::bail!(
            "sync_include root '{root}' is node-owned space under .ai/node; only {SYNC_INCLUDE_NODE_ALLOWED:?} may be declared there"
        );
    }
    Ok(())
}

/// The validated set of `.ai` roots AI-only project sync may carry. Built once
/// by the node and handed to every function that classifies, validates,
/// captures or applies project content. The floors stay `const` and are always
/// checked before this table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncInclude {
    entries: Vec<SyncIncludeEntry>,
}

impl SyncInclude {
    /// Validate each entry against the floors and refuse overlapping or
    /// duplicate roots. Entry order is preserved.
    pub fn new(entries: Vec<SyncIncludeEntry>) -> Result<Self> {
        for entry in &entries {
            validate_sync_include_entry(entry)?;
        }
        for (index, a) in entries.iter().enumerate() {
            for b in &entries[index + 1..] {
                if path_contains(&a.root, &b.root) || path_contains(&b.root, &a.root) {
                    anyhow::bail!("sync_include roots '{}' and '{}' overlap", a.root, b.root);
                }
            }
        }
        Ok(Self { entries })
    }

    /// Compose the node's table from the `sync_include` declarations of the
    /// installed bundles' signed manifests, each tagged with its bundle name.
    ///
    /// A `{kind}` declaration resolves through `kind_directory` (a kind's
    /// `location.directory`); an unknown kind is refused. Every resolved root
    /// is validated against the floors, and no two declarations from any
    /// bundles may overlap. The result is ordered by root, so the table does
    /// not depend on bundle install order.
    pub fn compose(
        bundles: &[(String, Vec<SyncIncludeDecl>)],
        kind_directory: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let mut resolved: Vec<(&str, SyncIncludeEntry)> = Vec::new();
        for (bundle, decls) in bundles {
            for decl in decls {
                let entry = decl.resolve(kind_directory).map_err(|error| {
                    anyhow::anyhow!("bundle `{bundle}` sync_include: {error:#}")
                })?;
                resolved.push((bundle.as_str(), entry));
            }
        }
        for (index, (left_bundle, left)) in resolved.iter().enumerate() {
            for (right_bundle, right) in &resolved[index + 1..] {
                if path_contains(&left.root, &right.root) || path_contains(&right.root, &left.root)
                {
                    anyhow::bail!(
                        "sync_include roots overlap: bundle `{left_bundle}` declares '{}' and bundle `{right_bundle}` declares '{}'",
                        left.root,
                        right.root
                    );
                }
            }
        }
        let mut entries = resolved
            .into_iter()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.root.cmp(&right.root));
        Self::new(entries)
    }

    /// The 19 rows the core bundle's manifest composes to, for tests that need
    /// a populated table without composing one from bundles.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests() -> Self {
        let mut entries = vec![
            SyncIncludeEntry::file(".ai/manifest.source.yaml"),
            SyncIncludeEntry::file(".ai/manifest.yaml"),
        ];
        entries.extend(
            [
                ".ai/directives",
                ".ai/tools",
                ".ai/graphs",
                ".ai/knowledge",
                ".ai/parsers",
                ".ai/handlers",
                ".ai/protocols",
                ".ai/revisions",
                ".ai/node/engine/kinds",
                ".ai/node/commands",
                ".ai/config/agent",
                ".ai/config/execution",
                ".ai/config/directive-runtime",
                ".ai/config/ryeos-runtime",
                ".ai/config/development",
                ".ai/config/schedules",
            ]
            .into_iter()
            .map(SyncIncludeEntry::directory),
        );
        entries.sort_by(|left, right| left.root.cmp(&right.root));
        Self::new(entries).expect("test sync_include table is valid")
    }

    pub fn entries(&self) -> &[SyncIncludeEntry] {
        &self.entries
    }

    /// The entry whose file or directory contains `rel_path`.
    pub fn entry_for_path(&self, rel_path: &str) -> Option<&SyncIncludeEntry> {
        self.entries
            .iter()
            .find(|entry| entry.matches_path(rel_path))
    }

    /// Entries atomically materialized to the live project during apply.
    /// Callers must respect each entry's exact file/directory shape.
    pub fn materialized(&self) -> impl Iterator<Item = &SyncIncludeEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.materialize_to_project)
    }

    /// True when a relative path is inside one of the managed sync roots.
    pub fn is_sync_path(&self, rel_path: &str) -> bool {
        self.entry_for_path(rel_path).is_some()
    }

    /// True when a relative path is exactly one of the directory roots.
    pub fn is_sync_root(&self, rel_path: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.shape == SyncIncludeShape::Directory && rel_path == entry.root)
    }
}

/// One `sync_include` entry as a bundle declares it in its signed manifest:
/// either `{kind: <item kind>}` (the root is that kind's `location.directory`
/// under `.ai`, a directory) or `{root: <path>, shape: file|directory}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncIncludeDecl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<SyncIncludeShape>,
    #[serde(
        default = "default_materialize_to_project",
        skip_serializing_if = "is_true"
    )]
    pub materialize_to_project: bool,
}

fn default_materialize_to_project() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl SyncIncludeDecl {
    /// The form rules that need no kind registry: exactly one of `kind` or
    /// `root` + `shape`, and a legal root. Unknown kinds are caught when the
    /// declaration is resolved against the installed kind registry.
    pub fn validate_form(&self) -> Result<()> {
        match (&self.kind, &self.root, &self.shape) {
            (Some(kind), None, None) => {
                if kind.is_empty()
                    || !kind
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                {
                    anyhow::bail!("kind name '{kind}' is not a valid item kind");
                }
                Ok(())
            }
            (None, Some(root), Some(shape)) => validate_sync_include_entry(&SyncIncludeEntry {
                root: root.clone(),
                shape: *shape,
                materialize_to_project: self.materialize_to_project,
            }),
            _ => anyhow::bail!(
                "a declaration is either `kind: <item kind>` or `root: <path>` with `shape: file|directory`, not both and not neither"
            ),
        }
    }

    /// Resolve to a validated entry. `kind_directory` maps a kind to its
    /// `location.directory`.
    pub fn resolve(
        &self,
        kind_directory: &dyn Fn(&str) -> Option<String>,
    ) -> Result<SyncIncludeEntry> {
        self.validate_form()?;
        let entry = match (&self.kind, &self.root, &self.shape) {
            (Some(kind), _, _) => {
                let directory =
                    kind_directory(kind).ok_or_else(|| anyhow::anyhow!("unknown kind '{kind}'"))?;
                SyncIncludeEntry {
                    root: format!(".ai/{directory}"),
                    shape: SyncIncludeShape::Directory,
                    materialize_to_project: self.materialize_to_project,
                }
            }
            (None, Some(root), Some(shape)) => SyncIncludeEntry {
                root: root.clone(),
                shape: *shape,
                materialize_to_project: self.materialize_to_project,
            },
            _ => unreachable!("validate_form admitted only the two declared forms"),
        };
        validate_sync_include_entry(&entry).map_err(|error| match &self.kind {
            Some(kind) => anyhow::anyhow!("kind '{kind}' resolves to a refused root: {error:#}"),
            None => error,
        })?;
        Ok(entry)
    }
}

/// Validate one bundle's own declarations: each well formed, and no two of its
/// explicit roots overlapping. Kinds are resolved later, against the installed
/// registry.
pub fn validate_sync_include_decls(decls: &[SyncIncludeDecl]) -> Result<()> {
    for decl in decls {
        decl.validate_form()?;
    }
    let roots = decls
        .iter()
        .filter_map(|decl| decl.root.as_deref())
        .collect::<Vec<_>>();
    for (index, left) in roots.iter().enumerate() {
        for right in &roots[index + 1..] {
            if path_contains(left, right) || path_contains(right, left) {
                anyhow::bail!("sync_include roots '{left}' and '{right}' overlap");
            }
        }
    }
    Ok(())
}

/// RyeOS-owned credential roots that must never leave the machine.
/// **Code-enforced structural floor**: this is independent from configurable
/// ignore policy and applies to every sync scope.
pub const NEVER_DEPLOY_SECRETS: &[&str] = &[
    ".ai/node/identity",
    ".ai/node/auth",
    ".ai/node/vault",
    // Frozen snapshot-floor entry: the floor is stored in every snapshot policy, so removing it would invalidate them.
    ".ai/config/keys/signing",
];

/// Node-owned runtime state that belongs to whichever node runs it (not project
/// content). **Code-enforced structural floor**: enforced for every sync scope
/// independently from configurable ignore policy. Deploying these would
/// clobber or leak the remote's own state.
pub const NODE_ROUTES_ROOT: &str = ".ai/node/routes";
pub const NODE_SCHEDULES_ROOT: &str = ".ai/node/schedules";
pub const NODE_BUNDLES_ROOT: &str = ".ai/node/bundles";
pub const BUNDLE_REGISTRY_LOCK: &str = ".ai/.bundles.lock";

pub const NODE_OWNED: &[&str] = &[
    ".ai/state",
    ".ai/cache",
    // Installed-bundle registry synchronization anchor. It is an empty
    // process lock, not project dependency metadata.
    BUNDLE_REGISTRY_LOCK,
    NODE_SCHEDULES_ROOT,
    NODE_ROUTES_ROOT,
    NODE_BUNDLES_ROOT,
];

/// Node-owned trust registry, operator and authoring key material and remote pins. Live
/// execution must never read or write them. They are not part of the project
/// snapshot floor: the floor identity is stored in every snapshot policy, so
/// widening it would invalidate existing policies.
pub const LIVE_EXECUTION_DENIED_ONLY: &[&str] = &[
    ".ai/node/trust",
    ".ai/node/operator",
    ".ai/node/authoring",
    ".ai/node/remotes",
];

/// True when a `.ai/`-relative path is inside one of the node-owned roots that
/// only the live-execution floor covers.
pub fn is_live_execution_denied_only_path(rel_path: &str) -> bool {
    matched_prefix(rel_path, LIVE_EXECUTION_DENIED_ONLY).is_some()
}

/// Central, non-bypassable live-execution control-plane floor. The same
/// classification owns snapshot/deploy safety; live execution serializes this
/// exact policy into its authority rather than duplicating path literals in an
/// executor or adapter.
pub fn live_execution_denied_control_paths() -> Vec<String> {
    let mut paths = NEVER_DEPLOY_SECRETS
        .iter()
        .chain(NODE_OWNED.iter())
        .chain(LIVE_EXECUTION_DENIED_ONLY.iter())
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

/// Canonical identity of every non-bypassable rule applied before bytes may
/// enter durable CAS. Keep this beside the predicate so import request
/// identity and capture enforcement cannot drift into separate hard-coded
/// path lists.
pub fn durable_content_capture_floor_rules() -> Vec<String> {
    snapshot_floor_rules()
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

/// Structural floor for content copied into durable CAS rather than merely
/// read live. Conventional exclusions are supplied separately by the exact
/// signed node policy captured with the snapshot.
pub fn is_durable_content_capture_floor_excluded(rel_path: &str) -> bool {
    is_project_snapshot_floor_excluded(rel_path)
}

fn sync_include_shape_str(shape: SyncIncludeShape) -> &'static str {
    match shape {
        SyncIncludeShape::File => "file",
        SyncIncludeShape::Directory => "directory",
    }
}

/// Render a read-only, human/LLM-readable view of the effective sync policy:
/// the `sync_include` table and the two code-enforced floors, plus a pointer to
/// the one editable input (`ignore_source`). This is a **discovery aid only** —
/// editing the generated file changes nothing; floors and the table are
/// enforced in code, and ignore is controlled by `ignore_source`.
pub fn render_effective_sync_policy_yaml(ignore_source: &str, include: &SyncInclude) -> String {
    let mut out = String::new();
    out.push_str("# GENERATED — read-only view of this node's effective sync policy.\n");
    out.push_str("# Editing this file does NOTHING. To change what is ignored, edit the file\n");
    out.push_str(&format!(
        "# named in `ignore_source` below ({ignore_source}).\n"
    ));
    out.push_str("# Secrets, node-owned state, and sync_include are enforced in code\n");
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
    out.push_str("# Project content AI-only sync carries.\n");
    out.push_str("sync_include:\n");
    for s in include.entries() {
        out.push_str(&format!(
            "  - {{ root: {:?}, shape: {}, materialize_to_project: {} }}\n",
            s.root,
            sync_include_shape_str(s.shape),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectAiPathClass {
    /// Under a [`NEVER_DEPLOY_SECRETS`] prefix — credential, never ships.
    NeverDeploySecret { prefix: &'static str },
    /// Matched the ignore matcher (junk / environment-specific).
    Ignored,
    /// Under a [`NODE_OWNED`] prefix — node runtime state, never ships.
    NodeOwned { prefix: &'static str },
    /// Inside a `sync_include` entry.
    Deployable(SyncIncludeEntry),
    /// Under `.ai/` but not inside a `sync_include` entry.
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
    include: &SyncInclude,
) -> Result<()> {
    for rel_path in manifest.item_source_hashes.keys() {
        validate_project_manifest_path(rel_path, scope, ignore, include)?;
    }
    Ok(())
}

/// Validate a complete project tree against the exact immutable policy stored
/// beside its snapshot. Current node ignore configuration is intentionally not
/// consulted here.
///
/// This is an ADMISSION check: for an AI-only policy every `.ai` path must lie
/// inside `include`. Callers that merely re-read a tree already admitted on
/// this node use [`validate_stored_project_tree_paths`].
pub fn validate_project_tree_paths(
    tree: &ProjectTree,
    policy: &ProjectSnapshotPolicy,
    include: &SyncInclude,
) -> Result<()> {
    validate_project_tree_paths_inner(tree, policy, Some(include))
}

/// Validate a tree that was already admitted on this node and is being read
/// back as evidence. Checks everything [`validate_project_tree_paths`] does
/// except membership in the node's current `sync_include` table: the floors,
/// ignore rules, path safety and bounds still apply, and an AI-only tree must
/// still be `.ai` content. A bundle uninstall therefore cannot make a stored
/// head unloadable; new pushes and applies are judged against the current
/// table at their own admission point.
pub fn validate_stored_project_tree_paths(
    tree: &ProjectTree,
    policy: &ProjectSnapshotPolicy,
) -> Result<()> {
    validate_project_tree_paths_inner(tree, policy, None)
}

fn validate_project_tree_paths_inner(
    tree: &ProjectTree,
    policy: &ProjectSnapshotPolicy,
    include: Option<&SyncInclude>,
) -> Result<()> {
    policy.validate()?;
    if tree.files.len() > MAX_PROJECT_TREE_FILES {
        anyhow::bail!("project tree exceeds {MAX_PROJECT_TREE_FILES} regular files");
    }
    let mut directories = std::collections::BTreeSet::new();
    let matcher = policy.matcher()?;
    for rel_path in tree.files.keys() {
        validate_project_manifest_path_inner(rel_path, policy.sync_scope, Some(&matcher), include)?;
        let path = std::path::Path::new(rel_path);
        let directory_depth = path.components().count().saturating_sub(1);
        if directory_depth > MAX_PROJECT_TREE_DEPTH {
            anyhow::bail!(
                "project tree path exceeds maximum depth {MAX_PROJECT_TREE_DEPTH}: {rel_path}"
            );
        }
        let mut parent = path.parent();
        while let Some(directory) = parent {
            if directory.as_os_str().is_empty() {
                break;
            }
            directories.insert(directory.to_path_buf());
            parent = directory.parent();
        }
        if tree.files.len().saturating_add(directories.len()) > MAX_PROJECT_TREE_ENTRIES {
            anyhow::bail!("project tree exceeds {MAX_PROJECT_TREE_ENTRIES} filesystem entries");
        }
    }
    Ok(())
}

/// Apply the receiving node's current signed conventional-ignore authority to
/// an already validated transferred tree. Source and target policies need not
/// be identical: admission is their intersection over actual content, not a
/// cross-node policy-identity requirement.
pub fn validate_project_tree_against_target_ignore(
    tree: &ProjectTree,
    target: &IgnoreMatcher,
) -> Result<()> {
    if let Some(path) = tree.files.keys().find(|path| target.is_ignored(path)) {
        anyhow::bail!(
            "project tree path '{}' is excluded by the target node's current signed ingest-ignore policy",
            path
        );
    }
    Ok(())
}

/// Validate a single manifest path for the declared sync scope.
pub fn validate_project_manifest_path(
    rel_path: &str,
    scope: ProjectSyncScope,
    ignore: Option<&IgnoreMatcher>,
    include: &SyncInclude,
) -> Result<()> {
    validate_project_manifest_path_inner(rel_path, scope, ignore, Some(include))
}

/// As [`validate_project_manifest_path`] for a path already admitted on this
/// node: no `sync_include` membership check.
pub fn validate_stored_project_manifest_path(
    rel_path: &str,
    scope: ProjectSyncScope,
    ignore: Option<&IgnoreMatcher>,
) -> Result<()> {
    validate_project_manifest_path_inner(rel_path, scope, ignore, None)
}

fn validate_project_manifest_path_inner(
    rel_path: &str,
    scope: ProjectSyncScope,
    ignore: Option<&IgnoreMatcher>,
    include: Option<&SyncInclude>,
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

    if ignore.is_some_and(|matcher| matcher.is_ignored(rel_path)) {
        anyhow::bail!(
            "project manifest path '{}' matches the captured node/project ignore policy and must not be in the manifest",
            rel_path
        );
    }

    if scope == ProjectSyncScope::AiOnly {
        match classify_project_ai_path_inner(rel_path, ignore, include) {
            ProjectAiPathClass::Deployable(entry)
                if entry.shape == SyncIncludeShape::Directory && rel_path == entry.root =>
            {
                anyhow::bail!(
                    "AI-only project manifest path '{}' names a sync_include root; expected a file below the root",
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
            // Already admitted here: membership is not re-judged.
            ProjectAiPathClass::UnknownAiPath if include.is_none() => {
                if let Some(reason) = ai_only_protected_refusal(rel_path) {
                    anyhow::bail!("AI-only project manifest path '{}' {reason}", rel_path);
                }
            }
            ProjectAiPathClass::UnknownAiPath => {
                anyhow::bail!(
                    "AI-only project manifest path '{}' is under .ai but outside sync_include",
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
    include: &SyncInclude,
) -> ProjectAiPathClass {
    classify_project_ai_path_inner(rel_path, ignore, Some(include))
}

fn classify_project_ai_path_inner(
    rel_path: &str,
    ignore: Option<&IgnoreMatcher>,
    include: Option<&SyncInclude>,
) -> ProjectAiPathClass {
    if let Some(prefix) = matched_prefix(rel_path, NEVER_DEPLOY_SECRETS) {
        return ProjectAiPathClass::NeverDeploySecret { prefix };
    }

    if let Some(m) = ignore
        && m.is_ignored(rel_path)
    {
        return ProjectAiPathClass::Ignored;
    }

    if let Some(prefix) = matched_prefix(rel_path, NODE_OWNED) {
        return ProjectAiPathClass::NodeOwned { prefix };
    }

    if let Some(entry) = include.and_then(|include| include.entry_for_path(rel_path)) {
        return ProjectAiPathClass::Deployable(entry.clone());
    }

    if rel_path == ".ai" || rel_path.starts_with(".ai/") {
        ProjectAiPathClass::UnknownAiPath
    } else {
        ProjectAiPathClass::NonAiPath
    }
}

/// Classify a path by the code-enforced floors alone: never-deploy secrets,
/// then node-owned state. No `sync_include` table or ignore matcher is
/// consulted.
pub fn classify_floor_path(rel_path: &str) -> Option<ProjectAiPathClass> {
    if let Some(prefix) = matched_prefix(rel_path, NEVER_DEPLOY_SECRETS) {
        return Some(ProjectAiPathClass::NeverDeploySecret { prefix });
    }
    matched_prefix(rel_path, NODE_OWNED).map(|prefix| ProjectAiPathClass::NodeOwned { prefix })
}

/// Basic relative path safety shared by full-project and AI-only snapshots.
pub fn validate_safe_relative_path(rel_path: &str) -> Result<()> {
    crate::objects::validate_canonical_project_relative_path(rel_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn inc() -> SyncInclude {
        SyncInclude::for_tests()
    }

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
            ".ai/manifest.source.yaml",
            ".ai/manifest.yaml",
            ".ai/directives/foo.md",
            ".ai/tools/app/tool.yaml",
            ".ai/graphs/app/flow.yaml",
            ".ai/config/execution/execution.yaml",
            ".ai/config/directive-runtime/limits.yaml",
            ".ai/config/development/ryeos/build-profile.yaml",
            ".ai/config/schedules/snap-track.yaml",
        ]);
        validate_project_manifest_paths(&m, ProjectSyncScope::AiOnly, None, &inc()).unwrap();
    }

    #[test]
    fn bundle_state_is_node_owned_never_synced() {
        for path in [
            ".ai/state/bundle-state",
            ".ai/state/bundle-state/central-auth/realm-a/session.json",
            ".ai/state/bundle-state/central-auth/.home.json",
        ] {
            assert!(
                is_project_snapshot_floor_excluded(path),
                "{path} must never be captured or deployed"
            );
            assert!(
                is_durable_content_capture_floor_excluded(path),
                "{path} must never be copied into CAS"
            );
            assert!(
                live_execution_denied_control_paths()
                    .iter()
                    .any(|denied| path.starts_with(denied.as_str())),
                "{path} is outside the live-execution denied control paths"
            );
            assert!(matches!(
                classify_project_ai_path(path, None, &inc()),
                ProjectAiPathClass::NodeOwned {
                    prefix: ".ai/state"
                }
            ));
        }
        // The project manifest validator refuses a manifest that carries a home.
        let m = manifest(&[".ai/state/bundle-state/central-auth/realm-a/x"]);
        assert!(
            validate_project_manifest_paths(&m, ProjectSyncScope::AiOnly, None, &inc()).is_err()
        );
    }

    #[test]
    fn live_execution_denies_the_node_trust_registry_outside_the_snapshot_floor() {
        assert!(
            live_execution_denied_control_paths()
                .iter()
                .any(|path| path == ".ai/node/trust")
        );
        assert!(!is_project_snapshot_floor_excluded(".ai/node/trust"));
    }

    #[test]
    fn ai_only_rejects_exact_managed_roots() {
        for path in [
            ".ai/directives",
            ".ai/tools",
            ".ai/graphs",
            ".ai/config/execution",
            ".ai/config/ryeos-runtime",
            ".ai/config/schedules",
        ] {
            let err = validate_project_manifest_paths(
                &manifest(&[path]),
                ProjectSyncScope::AiOnly,
                None,
                &inc(),
            )
            .expect_err("exact managed root path must be rejected");
            assert!(format!("{err:#}").contains("names a sync_include"));
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
            (".ai/services/project/apply.yaml", "outside sync_include"),
        ] {
            let err = validate_project_manifest_paths(
                &manifest(&[path]),
                ProjectSyncScope::AiOnly,
                None,
                &inc(),
            )
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
            ".ai/.bundles.lock",
            ".ai/node/routes/apply.yaml",
            "src/.ryeos-quarantine.1.2",
            "nested/.ryeos-pull-backup-1.2/journal.json",
        ] {
            validate_project_manifest_paths(
                &manifest(&[path]),
                ProjectSyncScope::FullProject,
                None,
                &inc(),
            )
            .expect_err("secret/node-owned path must be rejected for full_project");
        }
    }

    #[test]
    fn public_development_fixture_is_policy_data_not_secret_authority() {
        let fixture = manifest(&[".dev-keys/PUBLISHER_DEV.pem"]);
        validate_project_manifest_paths(&fixture, ProjectSyncScope::FullProject, None, &inc())
            .expect("tracked public development fixture is ordinary project content");

        let ignore = crate::ignore::IgnoreMatcher::from_config(&crate::ignore::IgnoreConfig {
            patterns: vec![".dev-keys/".to_owned()],
        })
        .unwrap();
        validate_project_manifest_paths(
            &fixture,
            ProjectSyncScope::FullProject,
            Some(&ignore),
            &inc(),
        )
        .expect_err("an exact signed node policy may exclude the fixture");
    }

    #[test]
    fn target_policy_intersects_content_without_requiring_policy_identity() {
        let tree = ProjectTree {
            files: std::collections::BTreeMap::from([("src/lib.rs".to_owned(), "ab".repeat(32))]),
        };
        let different_but_safe =
            crate::ignore::IgnoreMatcher::from_config(&crate::ignore::IgnoreConfig {
                patterns: vec!["target/".to_owned()],
            })
            .unwrap();
        validate_project_tree_against_target_ignore(&tree, &different_but_safe).unwrap();

        let excludes_transferred_path =
            crate::ignore::IgnoreMatcher::from_config(&crate::ignore::IgnoreConfig {
                patterns: vec!["*.rs".to_owned()],
            })
            .unwrap();
        validate_project_tree_against_target_ignore(&tree, &excludes_transferred_path)
            .expect_err("target policy must reject matching transferred content");
    }

    #[test]
    fn ignored_file_inside_surface_is_not_deployable() {
        let ignore = crate::ignore::IgnoreMatcher::from_config(&crate::ignore::IgnoreConfig {
            patterns: vec!["__pycache__/".to_owned()],
        })
        .unwrap();
        // A junk file inside a deployable surface classifies as Ignored, not
        // Deployable — ignore wins over the surface allowlist.
        assert_eq!(
            classify_project_ai_path(".ai/tools/app/__pycache__/x.pyc", Some(&ignore), &inc()),
            ProjectAiPathClass::Ignored
        );
    }

    #[test]
    fn classifies_project_ai_paths() {
        assert_eq!(
            classify_project_ai_path(".env", None, &inc()),
            ProjectAiPathClass::NonAiPath
        );
        assert!(matches!(
            classify_project_ai_path(".ai/.bundles.lock", None, &inc()),
            ProjectAiPathClass::NodeOwned {
                prefix: ".ai/.bundles.lock"
            }
        ));
        assert!(matches!(
            classify_project_ai_path(".ai/config/schedules/snap-track.yaml", None, &inc()),
            ProjectAiPathClass::Deployable(_)
        ));
        assert!(matches!(
            classify_project_ai_path(".ai/graphs/snap-track/show_rescrape.yaml", None, &inc()),
            ProjectAiPathClass::Deployable(_)
        ));
        assert!(matches!(
            classify_project_ai_path(".ai/config/execution/execution.yaml", None, &inc()),
            ProjectAiPathClass::Deployable(_)
        ));
        assert!(matches!(
            classify_project_ai_path(".ai/node/schedules/foo.yaml", None, &inc()),
            ProjectAiPathClass::NodeOwned {
                prefix: ".ai/node/schedules"
            }
        ));
        assert!(matches!(
            classify_project_ai_path(".ai/node/identity/key.pem", None, &inc()),
            ProjectAiPathClass::NeverDeploySecret {
                prefix: ".ai/node/identity"
            }
        ));
        assert_eq!(
            classify_project_ai_path(".ai/unknown/foo.yaml", None, &inc()),
            ProjectAiPathClass::UnknownAiPath
        );
        assert_eq!(
            classify_project_ai_path("src/index.ts", None, &inc()),
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
                &inc(),
            )
            .expect_err("unsafe full-project path must be rejected");
            validate_project_manifest_paths(
                &manifest(&[path]),
                ProjectSyncScope::AiOnly,
                None,
                &inc(),
            )
            .expect_err("unsafe ai-only path must be rejected");
        }
    }

    #[test]
    fn ai_only_does_not_prefix_match_spoofed_roots() {
        for path in [
            ".ai/directives-link/evil.md",
            ".ai/manifest.yaml/child",
            ".ai/manifest.yaml.backup",
        ] {
            validate_project_manifest_paths(
                &manifest(&[path]),
                ProjectSyncScope::AiOnly,
                None,
                &inc(),
            )
            .expect_err("prefix spoof must be rejected");
        }
    }

    #[test]
    fn exact_file_surfaces_are_not_directory_roots() {
        assert!(inc().is_sync_path(".ai/manifest.source.yaml"));
        assert!(inc().is_sync_path(".ai/manifest.yaml"));
        assert!(!inc().is_sync_root(".ai/manifest.source.yaml"));
        assert!(!inc().is_sync_root(".ai/manifest.yaml"));
        assert!(inc().is_sync_root(".ai/tools"));
    }

    #[test]
    fn rendered_policy_is_valid_yaml_with_all_buckets() {
        let yaml =
            render_effective_sync_policy_yaml(".ai/node/policies/ingest_ignore.yaml", &inc());
        let v: serde_yaml::Value =
            serde_yaml::from_str(&yaml).expect("generated policy is valid YAML");
        assert_eq!(v["version"].as_u64(), Some(1));
        assert_eq!(
            v["ignore_source"].as_str(),
            Some(".ai/node/policies/ingest_ignore.yaml")
        );
        let secrets = v["never_deploy_secrets"].as_sequence().unwrap();
        assert!(
            secrets
                .iter()
                .any(|x| x.as_str() == Some(".ai/node/identity"))
        );
        let node_owned = v["node_owned"].as_sequence().unwrap();
        assert!(node_owned.iter().any(|x| x.as_str() == Some(".ai/state")));
        assert!(
            node_owned
                .iter()
                .any(|x| x.as_str() == Some(BUNDLE_REGISTRY_LOCK))
        );
        assert_eq!(
            v["sync_include"].as_sequence().unwrap().len(),
            inc().entries().len()
        );
        let manifest = v["sync_include"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|entry| entry["root"].as_str() == Some(".ai/manifest.yaml"))
            .expect("generated manifest surface");
        assert_eq!(manifest["shape"].as_str(), Some("file"));
    }

    #[test]
    fn test_table_is_exactly_the_core_rows() {
        let mut expected: Vec<(&str, SyncIncludeShape)> = vec![
            (".ai/manifest.source.yaml", SyncIncludeShape::File),
            (".ai/manifest.yaml", SyncIncludeShape::File),
            (".ai/directives", SyncIncludeShape::Directory),
            (".ai/tools", SyncIncludeShape::Directory),
            (".ai/graphs", SyncIncludeShape::Directory),
            (".ai/knowledge", SyncIncludeShape::Directory),
            (".ai/parsers", SyncIncludeShape::Directory),
            (".ai/handlers", SyncIncludeShape::Directory),
            (".ai/protocols", SyncIncludeShape::Directory),
            (".ai/revisions", SyncIncludeShape::Directory),
            (".ai/node/engine/kinds", SyncIncludeShape::Directory),
            (".ai/node/commands", SyncIncludeShape::Directory),
            (".ai/config/agent", SyncIncludeShape::Directory),
            (".ai/config/execution", SyncIncludeShape::Directory),
            (".ai/config/directive-runtime", SyncIncludeShape::Directory),
            (".ai/config/ryeos-runtime", SyncIncludeShape::Directory),
            (".ai/config/development", SyncIncludeShape::Directory),
            (".ai/config/schedules", SyncIncludeShape::Directory),
        ];
        expected.sort();
        let table = inc();
        let actual: Vec<(&str, SyncIncludeShape)> = table
            .entries()
            .iter()
            .map(|entry| (entry.root.as_str(), entry.shape))
            .collect();
        assert_eq!(actual, expected, "the table is ordered by root");
        assert!(table.entries().iter().all(|e| e.materialize_to_project));
        assert_eq!(table.materialized().count(), 18);
    }

    fn refusal(entries: Vec<SyncIncludeEntry>) -> String {
        format!(
            "{:#}",
            SyncInclude::new(entries).expect_err("table must be refused")
        )
    }

    #[test]
    fn sync_include_refuses_roots_that_touch_a_floor() {
        for root in [
            // equal to a floor path
            ".ai/node/identity",
            ".ai/state",
            ".ai/node/trust",
            ".ai/node/remotes",
            // below a floor path
            ".ai/node/vault/x",
            ".ai/state/bundle-state",
            ".ai/node/operator/keys",
            // above a floor path
            ".ai",
            ".ai/node",
            ".ai/config",
            ".ai/config/keys",
        ] {
            let message = refusal(vec![SyncIncludeEntry::directory(root)]);
            assert!(
                message.contains("sync_include root") && !message.is_empty(),
                "{root}: {message}"
            );
        }
        assert!(refusal(vec![SyncIncludeEntry::file(".ai/.bundles.lock")]).contains("node-owned"));
        assert!(refusal(vec![SyncIncludeEntry::directory(".ai/node")]).contains("contains"));
    }

    #[test]
    fn sync_include_refuses_unsafe_or_non_ai_roots() {
        for root in [
            "src",
            "ai/tools",
            ".ai/../etc",
            "../.ai/tools",
            "/abs/.ai/tools",
            ".ai//tools",
            ".ai/./tools",
            ".ai/tools/",
            ".ai/a\\b",
            "",
        ] {
            SyncInclude::new(vec![SyncIncludeEntry::directory(root)])
                .expect_err(&format!("root {root:?} must be refused"));
        }
    }

    #[test]
    fn sync_include_refuses_overlapping_and_duplicate_roots() {
        assert!(
            refusal(vec![
                SyncIncludeEntry::directory(".ai/views"),
                SyncIncludeEntry::directory(".ai/views"),
            ])
            .contains("overlap")
        );
        assert!(
            refusal(vec![
                SyncIncludeEntry::directory(".ai/views"),
                SyncIncludeEntry::directory(".ai/views/sub"),
            ])
            .contains("overlap")
        );
        assert!(
            refusal(vec![
                SyncIncludeEntry::file(".ai/views/x.yaml"),
                SyncIncludeEntry::directory(".ai/views"),
            ])
            .contains("overlap")
        );
        // Sibling roots sharing a name prefix do not overlap.
        SyncInclude::new(vec![
            SyncIncludeEntry::directory(".ai/views"),
            SyncIncludeEntry::directory(".ai/views-extra"),
        ])
        .unwrap();
    }

    #[test]
    fn floors_are_checked_before_the_table() {
        // Even a table entry that sits next to a floor cannot reach into it.
        let table =
            SyncInclude::new(vec![SyncIncludeEntry::directory(".ai/node/engine/kinds")]).unwrap();
        assert!(matches!(
            classify_project_ai_path(".ai/node/vault/x", None, &table),
            ProjectAiPathClass::NeverDeploySecret { .. }
        ));
        assert_eq!(
            classify_project_ai_path(".ai/tools/x.py", None, &table),
            ProjectAiPathClass::UnknownAiPath
        );
    }

    #[test]
    fn rendered_policy_names_the_section_sync_include() {
        let yaml = render_effective_sync_policy_yaml("x", &inc());
        assert!(yaml.contains("\nsync_include:\n"));
        assert!(!yaml.contains("deployable_surfaces"));
    }

    #[test]
    fn stored_validation_keeps_floors_but_does_not_rejudge_membership() {
        // A path outside any table entry: refused at admission, accepted when
        // re-read as already-admitted evidence.
        let outside = ".ai/views/x.yaml";
        validate_project_manifest_path(outside, ProjectSyncScope::AiOnly, None, &inc())
            .expect_err("admission refuses a path outside the table");
        validate_stored_project_manifest_path(outside, ProjectSyncScope::AiOnly, None)
            .expect("a stored tree is not re-judged against the current table");
        // Floors, safe-path rules and the non-.ai refusal still apply.
        for path in [".ai/node/vault/k", ".ai/state/x", "src/main.rs", "../x"] {
            validate_stored_project_manifest_path(path, ProjectSyncScope::AiOnly, None)
                .expect_err(path);
        }
    }

    fn kinds(kind: &str) -> Option<String> {
        match kind {
            "tool" => Some("tools".to_owned()),
            "view" => Some("views".to_owned()),
            "config" => Some("config".to_owned()),
            "node" => Some("node".to_owned()),
            _ => None,
        }
    }

    fn decls(yaml: &str) -> Vec<SyncIncludeDecl> {
        serde_yaml::from_str(yaml).expect("declarations parse")
    }

    #[test]
    fn declarations_resolve_kinds_and_explicit_roots() {
        let table = SyncInclude::compose(
            &[(
                "core".to_owned(),
                decls(
                    "- kind: tool\n- root: .ai/manifest.yaml\n  shape: file\n- root: .ai/config/agent\n  shape: directory\n  materialize_to_project: false\n",
                ),
            )],
            &kinds,
        )
        .unwrap();
        let roots: Vec<_> = table
            .entries()
            .iter()
            .map(|e| (e.root.as_str(), e.shape, e.materialize_to_project))
            .collect();
        assert_eq!(
            roots,
            vec![
                (".ai/config/agent", SyncIncludeShape::Directory, false),
                (".ai/manifest.yaml", SyncIncludeShape::File, true),
                (".ai/tools", SyncIncludeShape::Directory, true),
            ]
        );
    }

    #[test]
    fn declarations_refuse_unknown_kinds_malformed_forms_and_floor_kinds() {
        let one = |yaml: &str| {
            format!(
                "{:#}",
                SyncInclude::compose(&[("b".to_owned(), decls(yaml))], &kinds)
                    .expect_err("declaration must be refused")
            )
        };
        assert!(one("- kind: nope\n").contains("unknown kind 'nope'"));
        assert!(one("- kind: tool\n  root: .ai/tools\n  shape: directory\n").contains("not both"));
        assert!(one("- root: .ai/tools\n").contains("not both and not neither"));
        assert!(one("- shape: file\n").contains("neither"));
        // A kind whose directory is wider than what may travel is refused by the
        // floor rules, not by a special case.
        assert!(one("- kind: config\n").contains("contains"));
        assert!(one("- kind: node\n").contains("contains"));
        assert!(
            one("- root: .ai/node/trust\n  shape: directory\n").contains("live-execution-denied")
        );
        assert!(
            serde_yaml::from_str::<Vec<SyncIncludeDecl>>("- kind: tool\n  surprise: 1\n").is_err(),
            "unknown fields are refused"
        );
    }

    #[test]
    fn composing_refuses_overlap_across_bundles_and_names_both() {
        let error = SyncInclude::compose(
            &[
                ("one".to_owned(), decls("- kind: tool\n")),
                (
                    "two".to_owned(),
                    decls("- root: .ai/tools/extra\n  shape: directory\n"),
                ),
            ],
            &kinds,
        )
        .expect_err("overlap");
        let message = format!("{error:#}");
        assert!(
            message.contains("`one`") && message.contains("`two`"),
            "{message}"
        );
    }

    #[test]
    fn bundle_local_validation_catches_overlap_without_a_registry() {
        validate_sync_include_decls(&decls("- kind: tool\n- kind: view\n")).unwrap();
        validate_sync_include_decls(&decls(
            "- root: .ai/a\n  shape: directory\n- root: .ai/a/b\n  shape: directory\n",
        ))
        .expect_err("overlap");
    }

    #[test]
    fn declarations_cannot_reach_node_owned_space_or_key_material() {
        for root in [
            ".ai/node/policies",
            ".ai/node/native-capacity",
            ".ai/node/external_execution",
            ".ai/node/offline-reset-1",
            ".ai/node/runtime_snapshot_x",
            ".ai/node/engine/bundles",
            ".ai/node/engine",
            ".ai/node/sync/policy.yaml",
            ".ai/config/keys/other",
            ".ai/config/keys",
        ] {
            SyncInclude::new(vec![SyncIncludeEntry::directory(root)])
                .expect_err(&format!("{root} must be refused"));
        }
        // The two allowed node roots, and a path below one, stay legal.
        SyncInclude::new(vec![
            SyncIncludeEntry::directory(".ai/node/engine/kinds"),
            SyncIncludeEntry::directory(".ai/node/commands/extra"),
        ])
        .unwrap();
    }

    #[test]
    fn stored_ai_only_validation_refuses_protected_and_node_space_but_not_table_membership() {
        for path in [
            ".ai/node/trust/x.yaml",
            ".ai/node/operator/k",
            ".ai/node/authoring/k",
            ".ai/node/remotes/r",
            ".ai/node/policies/p.yaml",
            ".ai/config/keys/other/k.pem",
        ] {
            validate_stored_project_manifest_path(path, ProjectSyncScope::AiOnly, None)
                .expect_err(path);
        }
        // Outside the table but not protected: still not re-judged.
        validate_stored_project_manifest_path(".ai/views/x.yaml", ProjectSyncScope::AiOnly, None)
            .unwrap();
        validate_stored_project_manifest_path(
            ".ai/node/commands/x.yaml",
            ProjectSyncScope::AiOnly,
            None,
        )
        .unwrap();
        // Full-project trees are untouched by the AI-only rule.
        validate_stored_project_manifest_path(
            ".ai/node/trust/x.yaml",
            ProjectSyncScope::FullProject,
            None,
        )
        .unwrap();
    }
}
