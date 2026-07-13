use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ryeos_engine::contracts::{SignatureEnvelope, TrustClass};
use ryeos_engine::trust::TrustStore;
use serde::{Deserialize, Serialize};

/// The only signed bundle-manifest format accepted by this release.
///
/// V1 predates an on-wire format discriminator, so it is identified by its
/// closed structural schema. A future format must use a new parser and all
/// signed manifests must be republished; this parser never upgrades in place.
pub const CURRENT_BUNDLE_MANIFEST_FORMAT: &str = "ryeos.bundle-manifest/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum BundleEventOperation {
    Append,
    Scan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeVaultOperation {
    Put,
    Get,
    Delete,
    List,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BundleEventDecl {
    pub event_kind: String,
    pub operations: Vec<BundleEventOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeVaultDecl {
    pub namespace: String,
    pub operations: Vec<RuntimeVaultOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ItemAuthorDecl {
    pub kind: String,
    pub namespace: String,
}

/// The single manifest-declared runtime-authority surface: the closed set of
/// daemon callback authority families a signed bundle manifest may grant its own
/// running code. One field per family, each a typed declaration list that owns
/// its own cap construction (see the impls in `runtime_authority`). Adding a
/// family is one field here plus one arm in the family-set behavior — every
/// generic caller (minting, doctor, publish, composition) folds over the set
/// rather than naming each family, so ancillary paths cannot silently drift.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAuthorityDecls {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundle_events: Vec<BundleEventDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_vault: Vec<RuntimeVaultDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub item_authoring: Vec<ItemAuthorDecl>,
}

/// One declared smoke execution: an item the bundle's author nominates as a
/// liveness probe for `ryeos bundle smoke`. Each entry is executed as a normal
/// thread against the bundle source with a temporary runtime state root.
///
/// `inputs` is intentionally open (`serde_json::Value`) — it is passed verbatim
/// as the execution's `parameters`, so its shape is the target item's input
/// schema, not the manifest's.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SmokeDecl {
    /// Canonical ref of the item to execute (e.g. `tool:example/system/health`).
    #[serde(rename = "ref")]
    pub item_ref: String,
    /// Optional label used in the smoke report; defaults to `item_ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Execution parameters passed verbatim; defaults to no inputs.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub inputs: serde_json::Value,
    /// Per-run client-side timeout. `None` defers to daemon-side limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl SmokeDecl {
    /// Label to report this smoke run under.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.item_ref)
    }
}

/// Validate a `smoke:` declaration list: every ref must look like a canonical
/// `<kind>:<bare-id>` ref and labels must be unique so report rows are
/// unambiguous.
pub fn validate_smoke_decls(decls: &[SmokeDecl]) -> Result<()> {
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for decl in decls {
        match decl.item_ref.split_once(':') {
            Some((kind, bare)) if !kind.trim().is_empty() && !bare.trim().is_empty() => {}
            _ => bail!(
                "invalid smoke ref '{}': expected a canonical `<kind>:<bare-id>` ref",
                decl.item_ref
            ),
        }
        if !seen.insert(decl.label()) {
            bail!("duplicate smoke entry label '{}'", decl.label());
        }
    }
    Ok(())
}

/// Validate a `shadows:` declaration list: every entry must be a canonical
/// `<kind>:<bare-id>` ref (so it can match a shipped item by exact ref) and
/// entries must be unique.
pub fn validate_shadow_decls(shadows: &[String]) -> Result<()> {
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for shadow in shadows {
        match shadow.split_once(':') {
            Some((kind, bare)) if !kind.trim().is_empty() && !bare.trim().is_empty() => {}
            _ => bail!(
                "invalid shadow ref '{}': expected a canonical `<kind>:<bare-id>` ref",
                shadow
            ),
        }
        if !seen.insert(shadow.as_str()) {
            bail!("duplicate shadow declaration '{}'", shadow);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BundleManifestSource {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub requires_kinds: Vec<String>,
    #[serde(default)]
    pub uses_kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "RuntimeAuthorityDecls::is_empty")]
    pub runtime_authority: RuntimeAuthorityDecls,
    /// Declared smoke executions for `ryeos bundle smoke`. Absent in most
    /// manifests; `skip_serializing_if` keeps re-serialized manifests that
    /// never declared it byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub smoke: Vec<SmokeDecl>,
    /// Declared config shadows: canonical refs this bundle deliberately ships
    /// under a foreign namespace to override another bundle's config via
    /// project-first resolution. Signed intent the namespace lint verifies
    /// against what the bundle actually ships, so a deliberate override is
    /// distinguishable from an accidental foreign-namespace item. Absent in
    /// most manifests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadows: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub provides_kinds: Vec<String>,
    #[serde(default)]
    pub requires_kinds: Vec<String>,
    #[serde(default)]
    pub uses_kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "RuntimeAuthorityDecls::is_empty")]
    pub runtime_authority: RuntimeAuthorityDecls,
    /// Declared smoke executions, carried verbatim from the source manifest
    /// so the generated (signed) manifest states them too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub smoke: Vec<SmokeDecl>,
    /// Declared config shadows, carried verbatim from the source manifest so
    /// the generated (signed) manifest states the override intent too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadows: Vec<String>,
}

pub(crate) fn parse_current_manifest_body(body: &str, origin: &Path) -> Result<BundleManifest> {
    let value: serde_yaml::Value = serde_yaml::from_str(body)
        .with_context(|| format!("parse {CURRENT_BUNDLE_MANIFEST_FORMAT} at {}", origin.display()))?;
    let mapping = value.as_mapping().ok_or_else(|| {
        anyhow::anyhow!(
            "{} must contain a YAML mapping in format {CURRENT_BUNDLE_MANIFEST_FORMAT}",
            origin.display()
        )
    })?;
    for required in ["name", "version", "provides_kinds", "requires_kinds"] {
        if !mapping.contains_key(&serde_yaml::Value::String(required.to_string())) {
            bail!(
                "{} is not {CURRENT_BUNDLE_MANIFEST_FORMAT}: missing required field '{required}'",
                origin.display()
            );
        }
    }
    serde_yaml::from_value(value).with_context(|| {
        format!(
            "validate {} as {CURRENT_BUNDLE_MANIFEST_FORMAT}",
            origin.display()
        )
    })
}

pub fn derive_provides_kinds(ai_dir: &Path) -> Result<Vec<String>> {
    let kinds_dir = ai_dir.join("node").join("engine").join("kinds");
    if !kinds_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut kinds: Vec<String> = Vec::new();
    for entry in fs::read_dir(&kinds_dir)
        .with_context(|| format!("read kinds dir {}", kinds_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let schema = kinds_dir
            .join(&name)
            .join(format!("{name}.kind-schema.yaml"));
        if schema.exists() {
            kinds.push(name);
        }
    }
    kinds.sort();
    kinds.dedup();
    Ok(kinds)
}

pub fn materialize_manifest(
    source: BundleManifestSource,
    ai_dir: &Path,
    expected_name: &str,
) -> Result<BundleManifest> {
    if source.name != expected_name {
        bail!(
            "manifest identity mismatch: source.name is '{}' but expected '{}' — \
             update manifest.source.yaml name to match the directory",
            source.name,
            expected_name
        );
    }
    source
        .runtime_authority
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid `runtime_authority` declaration: {e}"))?;
    validate_smoke_decls(&source.smoke)
        .map_err(|e| anyhow::anyhow!("invalid `smoke` declaration: {e}"))?;
    validate_shadow_decls(&source.shadows)
        .map_err(|e| anyhow::anyhow!("invalid `shadows` declaration: {e}"))?;
    let provides_kinds = derive_provides_kinds(ai_dir)?;
    Ok(BundleManifest {
        name: source.name,
        version: source.version,
        description: source.description,
        provides_kinds,
        requires_kinds: source.requires_kinds,
        uses_kinds: source.uses_kinds,
        runtime_authority: source.runtime_authority,
        smoke: source.smoke,
        shadows: source.shadows,
    })
}

pub fn load_verified_manifest_yaml(
    ai_dir: &Path,
    expected_name: Option<&str>,
    trust_store: &TrustStore,
) -> Result<Option<BundleManifest>> {
    let manifest_path = ai_dir.join("manifest.yaml");
    if !manifest_path.exists() {
        return Ok(None);
    }
    let file_type = fs::symlink_metadata(&manifest_path)
        .with_context(|| format!("failed to stat {}", manifest_path.display()))?
        .file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        bail!(
            "bundle manifest at {} is not a regular file (symlinks rejected)",
            manifest_path.display()
        );
    }

    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read manifest {}", manifest_path.display()))?;
    let envelope = yaml_signature_envelope();
    let sig_header = ryeos_engine::item_resolution::parse_signature_header(&raw, &envelope)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "manifest.yaml has no valid signature header — \
                 it must be generated by the publish pipeline"
            )
        })?;
    let (trust_class, _) =
        ryeos_engine::trust::verify_item_signature(&raw, &sig_header, &envelope, trust_store)
            .map_err(|e| anyhow::anyhow!("manifest.yaml signature verification failed: {e}"))?;
    if trust_class != TrustClass::Trusted {
        bail!(
            "manifest.yaml signer {} is not trusted (trust_class: {:?})",
            sig_header.signer_fingerprint,
            trust_class
        );
    }

    let body = lillux::signature::strip_signature_lines(&raw);
    let manifest = parse_current_manifest_body(&body, &manifest_path)?;
    manifest.runtime_authority.validate().map_err(|e| {
        anyhow::anyhow!(
            "invalid `runtime_authority` declaration in {}: {e}",
            manifest_path.display()
        )
    })?;
    if let Some(expected_name) =
        expected_name.filter(|expected_name| manifest.name != *expected_name)
    {
        bail!(
            "manifest identity mismatch: manifest.yaml name is '{}' but expected '{}'",
            manifest.name,
            expected_name
        );
    }
    Ok(Some(manifest))
}

fn yaml_signature_envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    }
}

pub fn parse_manifest(source: &Path, expected_name: &str) -> Result<Option<BundleManifest>> {
    let ai_dir = source.join(".ai");

    let manifest_path = ai_dir.join("manifest.yaml");
    if manifest_path.exists() {
        let raw = fs::read_to_string(&manifest_path)
            .with_context(|| format!("read manifest {}", manifest_path.display()))?;
        let body = lillux::signature::strip_signature_lines(&raw);
        let manifest = parse_current_manifest_body(&body, &manifest_path)?;
        manifest.runtime_authority.validate().map_err(|e| {
            anyhow::anyhow!(
                "invalid `runtime_authority` declaration in {}: {e}",
                manifest_path.display()
            )
        })?;
        if manifest.name != expected_name {
            bail!(
                "manifest identity mismatch: manifest.yaml name is '{}' but expected '{}' — \
                 regenerate the manifest",
                manifest.name,
                expected_name
            );
        }
        return Ok(Some(manifest));
    }

    let source_path = ai_dir.join("manifest.source.yaml");
    if source_path.exists() {
        let raw = fs::read_to_string(&source_path)
            .with_context(|| format!("read manifest source {}", source_path.display()))?;
        let src: BundleManifestSource = serde_yaml::from_str(&raw)
            .with_context(|| format!("parse manifest source {}", source_path.display()))?;
        let manifest = materialize_manifest(src, &ai_dir, expected_name)?;
        return Ok(Some(manifest));
    }

    Ok(None)
}

pub fn validate_manifest_dependencies(bundles: &[(String, PathBuf)]) -> Result<()> {
    let manifests = parse_all_manifests(bundles)?;

    let mut all_provides: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (_, mf) in &manifests {
        if let Some(m) = mf {
            for k in &m.provides_kinds {
                all_provides.insert(k.clone());
            }
        }
    }

    let mut missing: Vec<(String, Vec<String>)> = Vec::new();
    for (name, mf) in &manifests {
        let Some(m) = mf else { continue };
        let mut unsatisfied: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for req in m.requires_kinds.iter().chain(m.uses_kinds.iter()) {
            if m.provides_kinds.contains(req) {
                continue;
            }
            if !all_provides.contains(req) {
                unsatisfied.insert(req.clone());
            }
        }
        if !unsatisfied.is_empty() {
            missing.push((name.clone(), unsatisfied.into_iter().collect()));
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    let mut msg = "bundle dependency check failed:\n".to_string();
    for (name, kinds) in &missing {
        msg.push_str(&format!(
            "  bundle '{}' requires kinds not provided by any bundle: {}\n",
            name,
            kinds.join(", ")
        ));
    }
    msg.push_str(&format!(
        "\n  all provided kinds across bundles: {}",
        all_provides.iter().cloned().collect::<Vec<_>>().join(", ")
    ));
    bail!("{}", msg)
}

/// Sort discovered bundles into installation order based on manifest dependencies.
///
/// Bundles with no `requires_kinds` come first. Bundles whose requirements are
/// fully satisfied by earlier bundles come next. Falls back to alphabetical
/// order for bundles without manifests or when no dependency information exists.
///
/// Returns a topologically-sorted copy of `bundles`.
pub fn sort_bundles_by_dependency(bundles: &[(String, PathBuf)]) -> Result<Vec<(String, PathBuf)>> {
    if bundles.len() <= 1 {
        return Ok(bundles.to_vec());
    }

    let manifests = parse_all_manifests(bundles)?;

    // Build (name, external_kinds, provides_kinds) for each bundle.
    // Bundles without manifests get empty deps (treated as leaf nodes).
    let mut bundle_deps: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
    for (name, mf) in &manifests {
        match mf {
            Some(m) => {
                let external_kinds: Vec<String> = m
                    .requires_kinds
                    .iter()
                    .chain(m.uses_kinds.iter())
                    .filter(|k| !m.provides_kinds.contains(k))
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                bundle_deps.push((name.clone(), external_kinds, m.provides_kinds.clone()));
            }
            None => {
                bundle_deps.push((name.clone(), Vec::new(), Vec::new()));
            }
        }
    }

    // Kahn's algorithm for topological sort.
    // Build adjacency: bundle A must come before bundle B if B requires a kind that A provides.
    let n = bundle_deps.len();
    let mut in_degree = vec![0usize; n];

    // For each bundle, which kinds does it provide?
    let provides: Vec<std::collections::HashSet<String>> = bundle_deps
        .iter()
        .map(|(_, _, prov)| prov.iter().cloned().collect())
        .collect();

    // For each bundle, which other bundles must precede it?
    // edges[j] = set of indices that must come before j
    let mut edges: Vec<std::collections::HashSet<usize>> =
        vec![std::collections::HashSet::new(); n];

    for j in 0..n {
        for req in &bundle_deps[j].1 {
            for i in 0..n {
                if i != j && provides[i].contains(req) {
                    if edges[j].insert(i) {
                        in_degree[j] += 1;
                    }
                }
            }
        }
    }

    // Seed with zero-degree nodes, sorted alphabetically for determinism.
    let mut queue: std::collections::BinaryHeap<std::cmp::Reverse<(String, usize)>> =
        std::collections::BinaryHeap::new();
    for i in 0..n {
        if in_degree[i] == 0 {
            queue.push(std::cmp::Reverse((bundle_deps[i].0.clone(), i)));
        }
    }

    let mut sorted_indices: Vec<usize> = Vec::new();
    while let Some(std::cmp::Reverse((_, idx))) = queue.pop() {
        sorted_indices.push(idx);
        // For every other bundle j, if idx -> j was an edge, decrement in-degree.
        for j in 0..n {
            if edges[j].contains(&idx) {
                in_degree[j] -= 1;
                if in_degree[j] == 0 {
                    queue.push(std::cmp::Reverse((bundle_deps[j].0.clone(), j)));
                }
            }
        }
    }

    if sorted_indices.len() != n {
        // Cycle detected. This shouldn't happen with well-formed bundles.
        bail!(
            "circular dependency detected among bundles: {}",
            bundle_deps
                .iter()
                .map(|(n, _, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Map sorted indices back to the original (name, path) pairs.
    let name_to_bundle: std::collections::HashMap<String, (String, PathBuf)> = bundles
        .iter()
        .map(|(name, path)| (name.clone(), (name.clone(), path.clone())))
        .collect();

    let result: Vec<(String, PathBuf)> = sorted_indices
        .iter()
        .filter_map(|&idx| name_to_bundle.get(&bundle_deps[idx].0).cloned())
        .collect();

    Ok(result)
}

fn parse_all_manifests(
    bundles: &[(String, PathBuf)],
) -> Result<Vec<(String, Option<BundleManifest>)>> {
    let mut manifests: Vec<(String, Option<BundleManifest>)> = Vec::new();
    for (name, path) in bundles {
        let mf = parse_manifest(path, name)
            .with_context(|| format!("parse manifest for bundle {}", name))?;
        manifests.push((name.clone(), mf));
    }
    Ok(manifests)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("bundles").is_dir())
            .expect("workspace root with bundles/ directory")
            .to_path_buf()
    }

    fn is_valid_bundle_name(name: &str) -> bool {
        if name.is_empty() || name.len() > 64 {
            return false;
        }
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    }

    fn discover_bundles(source_dir: &Path) -> Result<Vec<(String, PathBuf)>> {
        if !source_dir.is_dir() {
            bail!("source directory does not exist: {}", source_dir.display());
        }

        let mut bundles = Vec::new();
        let entries = fs::read_dir(source_dir)
            .with_context(|| format!("read source directory {}", source_dir.display()))?;

        for entry in entries {
            let entry = entry.context("read source dir entry")?;
            let file_type = entry.file_type().context("read source dir entry type")?;
            if !file_type.is_dir() {
                continue;
            }

            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if name_str.starts_with('.') {
                continue;
            }

            if !is_valid_bundle_name(&name_str) {
                continue;
            }

            let child_path = entry.path();
            if child_path.join(ryeos_engine::AI_DIR).is_dir() {
                bundles.push((name_str.into_owned(), child_path));
            }
        }

        bundles.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(bundles)
    }

    #[test]
    fn parse_manifest_reads_core() {
        let mf = parse_manifest(&workspace_root().join("bundles/core"), "core")
            .expect("parse core manifest")
            .expect("core has a manifest");
        assert_eq!(mf.name, "core");
        assert_eq!(mf.version, "0.5.0");
        assert!(!mf.provides_kinds.is_empty());
        assert!(mf.provides_kinds.contains(&"config".to_string()));
        assert!(mf.provides_kinds.contains(&"handler".to_string()));
        assert!(mf.provides_kinds.contains(&"parser".to_string()));
        assert!(mf.provides_kinds.contains(&"runtime".to_string()));
        assert!(mf.provides_kinds.contains(&"service".to_string()));
        assert!(mf.provides_kinds.contains(&"tool".to_string()));
        assert!(
            !mf.provides_kinds.contains(&"knowledge".to_string()),
            "core must NOT provide knowledge after schema move to standard: {:?}",
            mf.provides_kinds
        );
        assert!(mf.requires_kinds.is_empty(), "core should have no requires");
    }

    #[test]
    fn parse_manifest_reads_standard() {
        let mf = parse_manifest(&workspace_root().join("bundles/standard"), "standard")
            .expect("parse standard manifest")
            .expect("standard has a manifest");
        assert_eq!(mf.name, "standard");
        assert!(mf.provides_kinds.contains(&"directive".to_string()));
        assert!(mf.provides_kinds.contains(&"graph".to_string()));
        assert!(
            mf.provides_kinds.contains(&"knowledge".to_string()),
            "standard must provide knowledge after schema move from core"
        );
        assert!(
            !mf.uses_kinds.contains(&"knowledge".to_string()),
            "standard must not use knowledge externally since it now provides it"
        );
        assert!(
            mf.requires_kinds.contains(&"config".to_string()),
            "standard requires config from core"
        );
        assert!(
            mf.requires_kinds.contains(&"handler".to_string()),
            "standard requires handler from core"
        );
    }

    #[test]
    fn parse_manifest_reads_hosted_node_from_source() {
        let root = workspace_root().join("bundles/hosted-node");
        let mf = parse_manifest(&workspace_root().join("bundles/hosted-node"), "hosted-node")
            .expect("parse hosted-node manifest")
            .expect("hosted-node has a manifest source");
        assert_eq!(mf.name, "hosted-node");
        assert_eq!(mf.version, "0.1.0");
        assert!(mf.provides_kinds.is_empty());
        assert_eq!(mf.requires_kinds, vec!["node".to_string()]);
        assert!(
            mf.uses_kinds.is_empty(),
            "hosted-node runtime bundle must stay core-only; docs belong outside .ai/"
        );
        assert!(root.join(".ai/node/hosted/policy.yaml").is_file());
        assert!(
            !root.join(".ai/config").exists(),
            "hosted-node runtime policy belongs under .ai/node/hosted, not .ai/config"
        );
        assert!(
            !root.join(".ai/knowledge").exists(),
            "hosted-node docs must stay outside .ai to avoid a standard/knowledge dependency"
        );
    }

    #[test]
    fn parse_manifest_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("no-manifest");
        fs::create_dir_all(bundle.join(".ai")).unwrap();
        assert!(parse_manifest(&bundle, "no-manifest").unwrap().is_none());
    }

    #[test]
    fn parse_manifest_rejects_invalid_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bad-manifest");
        fs::create_dir_all(bundle.join(".ai")).unwrap();
        fs::write(bundle.join(".ai/manifest.source.yaml"), "not: [valid\nyaml").unwrap();
        assert!(parse_manifest(&bundle, "bad-manifest").is_err());
    }

    #[test]
    fn validate_dependencies_core_and_standard_ok() {
        let bundles = discover_bundles(&workspace_root().join("bundles")).unwrap();
        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "core + standard should satisfy all dependencies"
        );
    }

    #[test]
    fn validate_dependencies_core_and_hosted_node_without_standard_ok() {
        let root = workspace_root();
        let bundles = vec![
            ("core".to_string(), root.join("bundles/core")),
            ("hosted-node".to_string(), root.join("bundles/hosted-node")),
        ];

        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "hosted-node must install with core only, without standard"
        );
    }

    #[test]
    fn validate_dependencies_fails_with_missing_provider() {
        let tmp = tempfile::tempdir().unwrap();

        let needy = tmp.path().join("needy");
        fs::create_dir_all(needy.join(".ai")).unwrap();
        fs::write(
            needy.join(".ai/manifest.source.yaml"),
            "name: needy\nversion: '1.0'\nrequires_kinds:\n  - magic\n",
        )
        .unwrap();

        let bundles = vec![("needy".to_string(), needy)];
        let err = validate_manifest_dependencies(&bundles).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("magic"),
            "error should mention missing kind 'magic': {msg}"
        );
        assert!(
            msg.contains("needy"),
            "error should mention bundle 'needy': {msg}"
        );
    }

    #[test]
    fn validate_dependencies_skips_bundles_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("bare");
        fs::create_dir_all(bare.join(".ai")).unwrap();

        let bundles = vec![("bare".to_string(), bare)];
        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "bundles without manifests should pass"
        );
    }

    #[test]
    fn validate_dependencies_self_provide_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let selfish = tmp.path().join("selfish");
        fs::create_dir_all(selfish.join(".ai/node/engine/kinds/foo")).unwrap();
        fs::write(
            selfish.join(".ai/node/engine/kinds/foo/foo.kind-schema.yaml"),
            "kind: config\ndirectory: foo\nextensions: []\n",
        )
        .unwrap();
        fs::write(
            selfish.join(".ai/manifest.source.yaml"),
            "name: selfish\nversion: '1.0'\nrequires_kinds:\n  - foo\n",
        )
        .unwrap();

        let bundles = vec![("selfish".to_string(), selfish)];
        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "self-providing bundle should pass"
        );
    }

    #[test]
    fn validate_dependencies_cross_bundle_satisfies() {
        let tmp = tempfile::tempdir().unwrap();

        let provider = tmp.path().join("provider");
        fs::create_dir_all(provider.join(".ai/node/engine/kinds/alpha")).unwrap();
        fs::write(
            provider.join(".ai/node/engine/kinds/alpha/alpha.kind-schema.yaml"),
            "kind: config\ndirectory: alpha\nextensions: []\n",
        )
        .unwrap();
        fs::write(
            provider.join(".ai/manifest.source.yaml"),
            "name: provider\nversion: '1.0'\nrequires_kinds: []\n",
        )
        .unwrap();

        let consumer = tmp.path().join("consumer");
        fs::create_dir_all(consumer.join(".ai")).unwrap();
        fs::write(
            consumer.join(".ai/manifest.source.yaml"),
            "name: consumer\nversion: '1.0'\nrequires_kinds:\n  - alpha\n",
        )
        .unwrap();

        let bundles = vec![
            ("consumer".to_string(), consumer),
            ("provider".to_string(), provider),
        ];
        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "cross-bundle dependency should be satisfied"
        );
    }

    #[test]
    fn manifest_name_must_match_bundle_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("real-name");
        fs::create_dir_all(bundle.join(".ai")).unwrap();
        fs::write(
            bundle.join(".ai/manifest.source.yaml"),
            "name: wrong-name\nversion: '1.0'\nrequires_kinds: []\n",
        )
        .unwrap();

        let err = parse_manifest(&bundle, "real-name").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("mismatch"),
            "error should mention mismatch: {msg}"
        );
        assert!(
            msg.contains("real-name") && msg.contains("wrong-name"),
            "error should name both: {msg}"
        );
    }

    #[test]
    fn manifest_rejects_unknown_fields() {
        let yaml = r#"
name: test
version: "1.0"
provides_kinds: []
requires_kinds: []
typo_field: oops
"#;
        let result: Result<BundleManifest, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "unknown field should be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unknown field"),
            "error should mention unknown field: {msg}"
        );
    }

    #[test]
    fn current_signed_manifest_format_requires_complete_v1_shape() {
        let origin = Path::new("manifest.yaml");
        let error = parse_current_manifest_body(
            "name: demo\nversion: 1.0.0\nprovides_kinds: []\n",
            origin,
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing required field 'requires_kinds'"));
    }

    #[test]
    fn manifest_rejects_old_flat_runtime_authority_fields() {
        // Hard switch: the runtime-authority families live under
        // `runtime_authority:` only. Old top-level siblings fail loudly rather
        // than silently dropping authority — no back-compat.
        for field in ["bundle_events", "runtime_vault", "item_authoring"] {
            let yaml = format!(
                "name: test\nversion: \"1.0\"\nprovides_kinds: []\nrequires_kinds: []\n{field}: []\n"
            );
            let manifest: Result<BundleManifest, _> = serde_yaml::from_str(&yaml);
            assert!(
                manifest.is_err(),
                "top-level `{field}:` must be rejected on a BundleManifest"
            );
            let source_yaml = format!("name: test\nversion: \"1.0\"\n{field}: []\n");
            let source: Result<BundleManifestSource, _> = serde_yaml::from_str(&source_yaml);
            assert!(
                source.is_err(),
                "top-level `{field}:` must be rejected on a BundleManifestSource"
            );
        }
    }

    #[test]
    fn derive_provides_kinds_scans_core_schemas() {
        let ai_dir = workspace_root().join("bundles/core/.ai");
        let kinds = derive_provides_kinds(&ai_dir).expect("derive core provides_kinds");
        assert!(
            kinds.contains(&"config".to_string()),
            "core must provide config: {kinds:?}"
        );
        assert!(
            kinds.contains(&"handler".to_string()),
            "core must provide handler: {kinds:?}"
        );
        assert!(
            kinds.contains(&"tool".to_string()),
            "core must provide tool: {kinds:?}"
        );
        assert!(
            !kinds.contains(&"directive".to_string()),
            "directive is a standard kind, not core: {kinds:?}"
        );
        assert!(
            !kinds.contains(&"knowledge".to_string()),
            "core must NOT provide knowledge after schema move to standard: {kinds:?}"
        );
    }

    #[test]
    fn derive_provides_kinds_scans_standard_schemas() {
        let ai_dir = workspace_root().join("bundles/standard/.ai");
        let kinds = derive_provides_kinds(&ai_dir).expect("derive standard provides_kinds");
        assert!(
            kinds.contains(&"directive".to_string()),
            "standard must provide directive: {kinds:?}"
        );
        assert!(
            kinds.contains(&"graph".to_string()),
            "standard must provide graph: {kinds:?}"
        );
        assert!(
            kinds.contains(&"knowledge".to_string()),
            "standard must provide knowledge after schema move from core: {kinds:?}"
        );
    }

    #[test]
    fn derive_provides_kinds_returns_empty_without_kinds_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let kinds = derive_provides_kinds(tmp.path()).unwrap();
        assert!(kinds.is_empty());
    }

    #[test]
    fn materialize_manifest_derives_provides_from_schemas() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("test-bundle");
        let ai_dir = bundle.join(".ai");
        fs::create_dir_all(ai_dir.join("node/engine/kinds/mykind")).unwrap();
        fs::write(
            ai_dir.join("node/engine/kinds/mykind/mykind.kind-schema.yaml"),
            "kind: config\ndirectory: mykind\nextensions: []\n",
        )
        .unwrap();

        let source = BundleManifestSource {
            name: "test-bundle".to_string(),
            version: "1.0".to_string(),
            description: "test".to_string(),
            requires_kinds: vec![],
            uses_kinds: vec![],
            runtime_authority: RuntimeAuthorityDecls::default(),
            smoke: vec![],
            shadows: vec![],
        };
        let manifest = materialize_manifest(source, &ai_dir, "test-bundle").unwrap();
        assert_eq!(manifest.provides_kinds, vec!["mykind"]);
        assert_eq!(manifest.name, "test-bundle");
    }

    #[test]
    fn materialize_manifest_rejects_invalid_runtime_authority_declaration() {
        // Declaration validation is enforced on the materialize path, not only at
        // launch/mint — a wildcard `event_kind` never reaches signing.
        let tmp = tempfile::tempdir().unwrap();
        let ai_dir = tmp.path().join("arc/.ai");
        fs::create_dir_all(&ai_dir).unwrap();
        let source = BundleManifestSource {
            name: "arc".to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            requires_kinds: vec![],
            uses_kinds: vec![],
            runtime_authority: RuntimeAuthorityDecls {
                bundle_events: vec![BundleEventDecl {
                    event_kind: "ev_*".to_string(),
                    operations: vec![BundleEventOperation::Append],
                }],
                ..Default::default()
            },
            smoke: vec![],
            shadows: vec![],
        };
        let err = materialize_manifest(source, &ai_dir, "arc").unwrap_err();
        assert!(err.to_string().contains("runtime_authority"), "got: {err}");
    }

    #[test]
    fn parse_manifest_dev_mode_materializes_from_source() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("dev-bundle");
        let ai_dir = bundle.join(".ai");
        fs::create_dir_all(ai_dir.join("node/engine/kinds/custom")).unwrap();
        fs::write(
            ai_dir.join("node/engine/kinds/custom/custom.kind-schema.yaml"),
            "kind: config\ndirectory: custom\nextensions: []\n",
        )
        .unwrap();
        fs::write(
            ai_dir.join("manifest.source.yaml"),
            "name: dev-bundle\nversion: '0.1'\ndescription: 'dev test'\nrequires_kinds: []\n",
        )
        .unwrap();

        let mf = parse_manifest(&bundle, "dev-bundle")
            .unwrap()
            .expect("should find manifest via source fallback");
        assert_eq!(mf.name, "dev-bundle");
        assert_eq!(mf.provides_kinds, vec!["custom"]);
        assert!(mf.requires_kinds.is_empty());
    }

    #[test]
    fn parse_manifest_prefers_generated_over_source() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("pub-bundle");
        let ai_dir = bundle.join(".ai");
        fs::create_dir_all(&ai_dir).unwrap();
        fs::write(
            ai_dir.join("manifest.yaml"),
            "name: pub-bundle\nversion: '2.0'\nprovides_kinds:\n  - published-kind\nrequires_kinds: []\n",
        )
        .unwrap();
        fs::write(
            ai_dir.join("manifest.source.yaml"),
            "name: pub-bundle\nversion: '1.0'\ndescription: 'old source'\nrequires_kinds: []\n",
        )
        .unwrap();

        let mf = parse_manifest(&bundle, "pub-bundle")
            .unwrap()
            .expect("should find manifest");
        assert_eq!(mf.version, "2.0", "should read generated, not source");
        assert_eq!(mf.provides_kinds, vec!["published-kind"]);
    }

    #[test]
    fn smoke_absent_parses_empty_and_reserializes_without_field() {
        let yaml = "name: test\nversion: \"1.0\"\n";
        let source: BundleManifestSource = serde_yaml::from_str(yaml).unwrap();
        assert!(source.smoke.is_empty());
        // `skip_serializing_if` keeps re-serialized manifests that never
        // declared `smoke:` free of the field.
        let out = serde_yaml::to_string(&source).unwrap();
        assert!(!out.contains("smoke"), "unexpected smoke key: {out}");
    }

    #[test]
    fn smoke_present_parses_refs_inputs_and_defaults() {
        let yaml = r#"
name: test
version: "1.0"
smoke:
  - ref: tool:example/system/health
  - ref: directive:example/probe
    name: probe
    inputs:
      url: "http://localhost"
      retries: 3
    timeout_secs: 120
"#;
        let source: BundleManifestSource = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(source.smoke.len(), 2);

        let first = &source.smoke[0];
        assert_eq!(first.item_ref, "tool:example/system/health");
        assert_eq!(first.label(), "tool:example/system/health");
        assert!(first.inputs.is_null());
        assert_eq!(first.timeout_secs, None);

        let second = &source.smoke[1];
        assert_eq!(second.label(), "probe");
        assert_eq!(second.inputs["retries"], 3);
        assert_eq!(second.timeout_secs, Some(120));

        assert!(validate_smoke_decls(&source.smoke).is_ok());
    }

    #[test]
    fn smoke_rejects_unknown_entry_fields() {
        let yaml = r#"
name: test
version: "1.0"
smoke:
  - ref: tool:example/health
    bogus: true
"#;
        let result: Result<BundleManifestSource, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_err(),
            "unknown smoke entry field must be rejected"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("bogus"), "error should name the field: {msg}");
    }

    #[test]
    fn smoke_rejects_entry_without_ref() {
        let yaml = "name: test\nversion: \"1.0\"\nsmoke:\n  - name: no-ref\n";
        let result: Result<BundleManifestSource, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_err(),
            "smoke entry without `ref:` must be rejected"
        );
    }

    #[test]
    fn smoke_validation_rejects_malformed_ref_and_duplicate_labels() {
        let decl = |item_ref: &str, name: Option<&str>| SmokeDecl {
            item_ref: item_ref.to_string(),
            name: name.map(str::to_string),
            inputs: serde_json::Value::Null,
            timeout_secs: None,
        };

        let err = validate_smoke_decls(&[decl("not-a-ref", None)]).unwrap_err();
        assert!(err.to_string().contains("canonical"), "{err}");

        let err = validate_smoke_decls(&[decl(":missing-kind", None)]).unwrap_err();
        assert!(err.to_string().contains("canonical"), "{err}");

        let err =
            validate_smoke_decls(&[decl("tool:a/b", Some("dup")), decl("tool:c/d", Some("dup"))])
                .unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn materialize_manifest_carries_and_validates_smoke() {
        let tmp = tempfile::tempdir().unwrap();
        let ai_dir = tmp.path().join("probe/.ai");
        fs::create_dir_all(&ai_dir).unwrap();

        let mut source = BundleManifestSource {
            name: "probe".to_string(),
            version: "0.1.0".to_string(),
            description: String::new(),
            requires_kinds: vec![],
            uses_kinds: vec![],
            runtime_authority: RuntimeAuthorityDecls::default(),
            smoke: vec![SmokeDecl {
                item_ref: "tool:probe/health".to_string(),
                name: None,
                inputs: serde_json::Value::Null,
                timeout_secs: None,
            }],
            shadows: vec![],
        };
        let manifest = materialize_manifest(source.clone(), &ai_dir, "probe").unwrap();
        assert_eq!(manifest.smoke, source.smoke);

        source.smoke[0].item_ref = "no-colon".to_string();
        let err = materialize_manifest(source, &ai_dir, "probe").unwrap_err();
        assert!(err.to_string().contains("smoke"), "{err}");
    }

    #[test]
    fn shadows_absent_parses_empty_and_reserializes_without_field() {
        let yaml = "name: test\nversion: \"1.0\"\n";
        let source: BundleManifestSource = serde_yaml::from_str(yaml).unwrap();
        assert!(source.shadows.is_empty());
        let out = serde_yaml::to_string(&source).unwrap();
        assert!(!out.contains("shadows"), "unexpected shadows key: {out}");
    }

    #[test]
    fn shadows_present_parses_and_materialize_carries_them() {
        let tmp = tempfile::tempdir().unwrap();
        let ai_dir = tmp.path().join("downstream/.ai");
        fs::create_dir_all(&ai_dir).unwrap();

        let yaml = r#"
name: downstream
version: "1.0"
shadows:
  - config:ryeos-runtime/execution
  - config:ryeos-runtime/limits
"#;
        let source: BundleManifestSource = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            source.shadows,
            vec![
                "config:ryeos-runtime/execution".to_string(),
                "config:ryeos-runtime/limits".to_string(),
            ]
        );

        let manifest = materialize_manifest(source.clone(), &ai_dir, "downstream").unwrap();
        assert_eq!(manifest.shadows, source.shadows);
    }

    #[test]
    fn shadows_validation_rejects_malformed_and_duplicate_refs() {
        let err = validate_shadow_decls(&["not-a-ref".to_string()]).unwrap_err();
        assert!(err.to_string().contains("canonical"), "{err}");

        let err = validate_shadow_decls(&[":missing-kind".to_string()]).unwrap_err();
        assert!(err.to_string().contains("canonical"), "{err}");

        let err = validate_shadow_decls(&["config:a/b".to_string(), "config:a/b".to_string()])
            .unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn materialize_rejects_malformed_shadow_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let ai_dir = tmp.path().join("downstream/.ai");
        fs::create_dir_all(&ai_dir).unwrap();
        let source = BundleManifestSource {
            name: "downstream".to_string(),
            version: "1.0".to_string(),
            description: String::new(),
            requires_kinds: vec![],
            uses_kinds: vec![],
            runtime_authority: RuntimeAuthorityDecls::default(),
            smoke: vec![],
            shadows: vec!["no-colon".to_string()],
        };
        let err = materialize_manifest(source, &ai_dir, "downstream").unwrap_err();
        assert!(err.to_string().contains("shadows"), "{err}");
    }

    #[test]
    fn source_rejects_unknown_fields() {
        let yaml = r#"
name: test
version: "1.0"
typo_field: oops
"#;
        let result: Result<BundleManifestSource, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_err(),
            "unknown field in source should be rejected"
        );
    }
}
