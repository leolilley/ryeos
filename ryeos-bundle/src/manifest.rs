use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifestSource {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub requires_kinds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub provides_kinds: Vec<String>,
    #[serde(default)]
    pub requires_kinds: Vec<String>,
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
        let schema = kinds_dir.join(&name).join(format!("{name}.kind-schema.yaml"));
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
    let provides_kinds = derive_provides_kinds(ai_dir)?;
    Ok(BundleManifest {
        name: source.name,
        version: source.version,
        description: source.description,
        provides_kinds,
        requires_kinds: source.requires_kinds,
    })
}

pub fn parse_manifest(source: &Path, expected_name: &str) -> Result<Option<BundleManifest>> {
    let ai_dir = source.join(".ai");

    let manifest_path = ai_dir.join("manifest.yaml");
    if manifest_path.exists() {
        let raw = fs::read_to_string(&manifest_path)
            .with_context(|| format!("read manifest {}", manifest_path.display()))?;
        let body = lillux::signature::strip_signature_lines(&raw);
        let manifest: BundleManifest = serde_yaml::from_str(&body)
            .with_context(|| format!("parse manifest {}", manifest_path.display()))?;
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

pub fn validate_manifest_dependencies(
    bundles: &[(String, PathBuf)],
) -> Result<()> {
    let mut manifests: Vec<(String, Option<BundleManifest>)> = Vec::new();
    for (name, path) in bundles {
        let mf = parse_manifest(path, name)
            .with_context(|| format!("parse manifest for bundle {}", name))?;
        manifests.push((name.clone(), mf));
    }

    let mut all_provides: std::collections::HashSet<String> =
        std::collections::HashSet::new();
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
        let mut unsatisfied: Vec<String> = Vec::new();
        for req in &m.requires_kinds {
            if !all_provides.contains(req) {
                unsatisfied.push(req.clone());
            }
        }
        if !unsatisfied.is_empty() {
            missing.push((name.clone(), unsatisfied));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
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
        let mf = parse_manifest(&workspace_root().join("ryeos-bundles/core"), "core")
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
        assert!(mf.requires_kinds.is_empty(), "core should have no requires");
    }

    #[test]
    fn parse_manifest_reads_standard() {
        let mf = parse_manifest(&workspace_root().join("ryeos-bundles/standard"), "standard")
            .expect("parse standard manifest")
            .expect("standard has a manifest");
        assert_eq!(mf.name, "standard");
        assert!(mf.provides_kinds.contains(&"directive".to_string()));
        assert!(mf.provides_kinds.contains(&"graph".to_string()));
        assert!(mf.provides_kinds.contains(&"knowledge".to_string()));
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
        let bundles = discover_bundles(&workspace_root().join("ryeos-bundles")).unwrap();
        assert!(
            validate_manifest_dependencies(&bundles).is_ok(),
            "core + standard should satisfy all dependencies"
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
    fn derive_provides_kinds_scans_core_schemas() {
        let ai_dir = workspace_root().join("ryeos-bundles/core/.ai");
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
    }

    #[test]
    fn derive_provides_kinds_scans_standard_schemas() {
        let ai_dir = workspace_root().join("ryeos-bundles/standard/.ai");
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
            "standard must provide knowledge: {kinds:?}"
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
        };
        let manifest = materialize_manifest(source, &ai_dir, "test-bundle").unwrap();
        assert_eq!(manifest.provides_kinds, vec!["mykind"]);
        assert_eq!(manifest.name, "test-bundle");
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
    fn source_rejects_unknown_fields() {
        let yaml = r#"
name: test
version: "1.0"
typo_field: oops
"#;
        let result: Result<BundleManifestSource, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "unknown field in source should be rejected");
    }
}
