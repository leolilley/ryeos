use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use std::sync::Arc;

use ryeos_engine::contracts::{InstanceViolationCode, SignatureEnvelope};
use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::item_resolution::parse_signature_header;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

use crate::manifest::{derive_provides_kinds, BundleManifest};

/// Severity of a preflight validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightIssueSeverity {
    /// Blocking — bundle verification fails.
    Error,
    /// Non-blocking — informational for authors.
    Warning,
}

/// A single validation issue found during bundle preflight.
#[derive(Debug, Clone)]
pub struct PreflightIssue {
    /// Relative path of the item within the bundle (e.g. `tools/my_tool.py`).
    pub item_path: String,
    /// Whether this blocks verification.
    pub severity: PreflightIssueSeverity,
    /// Machine-readable violation code.
    pub code: InstanceViolationCode,
    /// Dot-separated path within the item value (e.g. `launch.mode`).
    pub path: String,
    /// Human-readable description of what was expected.
    pub expected: String,
    /// Human-readable description of what was found.
    pub found: String,
}

pub fn preflight_verify_bundle(
    source_path: &Path,
    system_space_dir: &Path,
    user_root: Option<&Path>,
) -> Result<()> {
    // Runtime uses signed bundle registrations under `.ai/node/bundles/*.yaml`,
    // not raw `.ai/bundles/*` scans. Generic bundle install verifies against
    // the current verified installed set so new bundles can depend on already-
    // installed bundles without ambient unregistered state affecting preflight.
    let installed_bundle_roots: Vec<PathBuf> =
        crate::installed::load_installed_bundle_records(system_space_dir, user_root)
            .context("preflight: load installed bundle registrations")?
            .into_iter()
            .map(|record| record.bundle_root)
            .collect();
    preflight_verify_bundle_in_context(source_path, &installed_bundle_roots, user_root)
}

pub fn preflight_verify_bundle_in_context(
    source_path: &Path,
    dependency_bundle_roots: &[PathBuf],
    user_root: Option<&Path>,
) -> Result<()> {
    let ai_dir = source_path.join(ryeos_engine::AI_DIR);
    if !ai_dir.is_dir() {
        bail!("preflight: source has no .ai/ at {}", source_path.display());
    }

    let mut schema_roots = Vec::new();
    for root in dependency_bundle_roots {
        let kinds_dir = root
            .join(ryeos_engine::AI_DIR)
            .join(ryeos_engine::KIND_SCHEMAS_DIR);
        if kinds_dir.is_dir() {
            schema_roots.push(kinds_dir);
        }
    }
    let bundle_kinds = ai_dir.join(ryeos_engine::KIND_SCHEMAS_DIR);
    if bundle_kinds.is_dir() {
        schema_roots.push(bundle_kinds.clone());
    }
    if schema_roots.is_empty() {
        bail!(
            "preflight: no kind schemas in installed bundles or candidate bundle ({})",
            bundle_kinds.display()
        );
    }

    let trust_store = TrustStore::load_three_tier(None, user_root, &[])
        .context("preflight: load operator trust store")?;
    if trust_store.is_empty() {
        bail!(
            "preflight: operator trust store is empty — run `ryeos init` to \
             pin the platform author key, or `ryeos trust pin <fingerprint>` \
             to pin a third-party publisher"
        );
    }

    let kinds = KindRegistry::load_base(&schema_roots, &trust_store)
        .context("preflight: load kind schemas")?;

    let mut parser_search_roots: Vec<(PathBuf, ryeos_engine::resolution::TrustClass)> = Vec::new();
    let mut seen_roots: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let push_unique = |path: PathBuf,
                       trust: ryeos_engine::resolution::TrustClass,
                       roots: &mut Vec<(PathBuf, ryeos_engine::resolution::TrustClass)>,
                       seen: &mut std::collections::HashSet<PathBuf>| {
        let key = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            roots.push((path, trust));
        }
    };
    // Dependency bundles are trusted system roots selected by the caller's
    // verification plan. `init --source` passes source-bundle dependency
    // roots; generic install passes currently installed bundle roots.
    for root in dependency_bundle_roots {
        push_unique(
            root.clone(),
            ryeos_engine::resolution::TrustClass::TrustedSystem,
            &mut parser_search_roots,
            &mut seen_roots,
        );
    }
    if let Some(ur) = user_root {
        push_unique(
            ur.to_path_buf(),
            ryeos_engine::resolution::TrustClass::TrustedUser,
            &mut parser_search_roots,
            &mut seen_roots,
        );
    }
    // Candidate bundle being verified (last, so installed content takes precedence).
    push_unique(
        source_path.to_path_buf(),
        ryeos_engine::resolution::TrustClass::TrustedUser,
        &mut parser_search_roots,
        &mut seen_roots,
    );

    let legacy_trust = source_path
        .join(ryeos_engine::AI_DIR)
        .join("config/keys/trusted");
    if legacy_trust.is_dir() {
        tracing::warn!(
            path = %legacy_trust.display(),
            "bundle ships a legacy `.ai/config/keys/trusted/` dir which is \
             ignored — pin the publisher key with `ryeos trust pin <fingerprint>` \
             instead"
        );
    }
    let (parser_tools, _dups) = ParserRegistry::load_base(
        &parser_search_roots
            .iter()
            .map(|(p, _)| p.clone())
            .collect::<Vec<_>>(),
        &trust_store,
        &kinds,
    )
    .context("preflight: load parser tools")?;
    let handler_registry = HandlerRegistry::load_base(&parser_search_roots, &trust_store)
        .context("preflight: load handler descriptors")?;
    let parser_dispatcher = ParserDispatcher::new(parser_tools, Arc::new(handler_registry));

    let mut failures: Vec<String> = Vec::new();
    for kind_name in kinds.kinds() {
        let kind_schema = match kinds.get(kind_name) {
            Some(s) => s,
            None => continue,
        };
        let kind_dir = ai_dir.join(&kind_schema.directory);
        if !kind_dir.is_dir() {
            continue;
        }

        let mut files: Vec<PathBuf> = Vec::new();
        collect_files_recursive(&kind_dir, &mut files);

        for file_path in files {
            let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if kind_schema.spec_for(&format!(".{ext}")).is_none() {
                continue;
            }

            let rel = file_path.strip_prefix(&ai_dir).unwrap_or(&file_path);

            let content = match fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(e) => {
                    failures.push(format!("{}: read failed: {e}", rel.display()));
                    continue;
                }
            };

            let source_format = match kind_schema.resolved_format_for(&format!(".{ext}")) {
                Some(f) => f,
                None => {
                    failures.push(format!(
                        "{}: no source format for extension .{ext}",
                        rel.display()
                    ));
                    continue;
                }
            };

            let parsed = match parser_dispatcher.dispatch(
                &source_format.parser,
                &content,
                Some(&file_path),
                &source_format.signature,
            ) {
                Ok(v) => v,
                Err(e) => {
                    failures.push(format!("{}: parse failed: {e}", rel.display()));
                    continue;
                }
            };

            if let Err(e) = ryeos_engine::kind_registry::validate_metadata_anchoring(
                &parsed,
                &kind_schema.extraction_rules,
                &kind_schema.directory,
                &ai_dir,
                &file_path,
            ) {
                failures.push(format!("{}: {e}", rel.display()));
                continue;
            }

            let sig_header = ryeos_engine::item_resolution::parse_signature_header(
                &content,
                &source_format.signature,
            );
            match sig_header {
                Some(header) => {
                    if !trust_store.is_trusted(&header.signer_fingerprint) {
                        failures.push(format!(
                            "{}: signer {} not in operator trust store \
                             (run `ryeos trust pin {}` to trust this publisher)",
                            rel.display(),
                            header.signer_fingerprint,
                            header.signer_fingerprint
                        ));
                        continue;
                    }
                    if let Err(e) = ryeos_engine::trust::verify_item_signature(
                        &content,
                        &header,
                        &source_format.signature,
                        &trust_store,
                    ) {
                        failures.push(format!(
                            "{}: signature verification failed: {e}",
                            rel.display()
                        ));
                        continue;
                    }
                }
                None => {
                    failures.push(format!(
                        "{}: unsigned — all bundle items must be signed",
                        rel.display()
                    ));
                    continue;
                }
            }

            // ── Instance validation (identity-composed kinds only) ──
            // For kinds using the identity composer, the parsed value IS
            // the composed value, so we can validate it directly against
            // the kind's `composed_value_contract`. Extends-based kinds
            // are validated post-composition in the resolution pipeline
            // (Slice 2), so we skip them here.
            if kind_schema.composer == "handler:ryeos/core/identity" {
                let report = kind_schema
                    .composed_value_contract
                    .validate_instance(&parsed);
                for v in &report.errors {
                    failures.push(format!(
                        "{}: contract violation [{}] {}: expected {}, found {}",
                        rel.display(),
                        v.code,
                        v.path,
                        v.expected,
                        v.found,
                    ));
                }
                for v in &report.warnings {
                    tracing::warn!(
                        item = %rel.display(),
                        code = %v.code,
                        path = %v.path,
                        "preflight: {}",
                        format!("contract warning {}: expected {}, found {}", v.path, v.expected, v.found),
                    );
                }
            }
        }
    }

    if !failures.is_empty() {
        let mut msg = format!(
            "preflight verification failed for {} item(s):\n",
            failures.len()
        );
        for f in &failures {
            msg.push_str(&format!("  - {f}\n"));
        }
        bail!("{msg}");
    }

    verify_manifest_signature(&ai_dir, source_path, &trust_store)
        .context("preflight: bundle manifest verification")?;

    tracing::info!(
        source = %source_path.display(),
        "preflight verification passed"
    );
    Ok(())
}

pub fn verify_manifest_signature(
    ai_dir: &Path,
    source_path: &Path,
    trust_store: &TrustStore,
) -> Result<()> {
    let manifest_path = ai_dir.join("manifest.yaml");
    if !manifest_path.exists() {
        return Ok(());
    }

    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read manifest {}", manifest_path.display()))?;

    let envelope = SignatureEnvelope {
        prefix: "#".to_string(),
        suffix: None,
        after_shebang: false,
    };

    let sig_header = parse_signature_header(&raw, &envelope).ok_or_else(|| {
        anyhow::anyhow!(
            "manifest.yaml has no valid signature header — \
             it must be generated by the publish pipeline"
        )
    })?;

    if !trust_store.is_trusted(&sig_header.signer_fingerprint) {
        bail!(
            "manifest.yaml: signer {} not in operator trust store \
             (run `ryeos trust pin {}` to trust this publisher)",
            sig_header.signer_fingerprint,
            sig_header.signer_fingerprint
        );
    }

    ryeos_engine::trust::verify_item_signature(&raw, &sig_header, &envelope, trust_store)
        .map_err(|e| anyhow::anyhow!("manifest.yaml signature verification failed: {e}"))?;

    let body = lillux::signature::strip_signature_lines(&raw);
    let manifest: BundleManifest = serde_yaml::from_str(&body)
        .with_context(|| format!("parse manifest body from {}", manifest_path.display()))?;

    let dir_name = source_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("source_path has no directory name"))?;
    if manifest.name != dir_name {
        bail!(
            "manifest identity mismatch: manifest.yaml name is '{}' but \
             bundle directory is '{}' — the manifest must be regenerated",
            manifest.name,
            dir_name
        );
    }

    let actual_kinds =
        derive_provides_kinds(ai_dir).context("derive actual provides_kinds from kind schemas")?;
    let mut claimed = manifest.provides_kinds.clone();
    claimed.sort();
    claimed.dedup();
    let mut actual_normalized = actual_kinds.clone();
    actual_normalized.sort();
    actual_normalized.dedup();
    if claimed != actual_normalized {
        bail!(
            "manifest provides_kinds mismatch: manifest claims {:?} but \
             actual kind schemas on disk provide {:?} — regenerate the manifest \
             with the publish pipeline",
            manifest.provides_kinds,
            actual_kinds
        );
    }

    tracing::info!(
        name = %manifest.name,
        provides_kinds = ?manifest.provides_kinds,
        "manifest verification passed"
    );
    Ok(())
}

fn collect_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;

    struct BundleLayout {
        _tmp: tempfile::TempDir,
        source: PathBuf,
        ai_dir: PathBuf,
        signing_key: SigningKey,
        user_root: PathBuf,
    }

    impl BundleLayout {
        fn new(name: &str) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let source = tmp.path().join(name);
            let ai_dir = source.join(".ai");
            fs::create_dir_all(&ai_dir).unwrap();
            let signing_key = SigningKey::generate(&mut OsRng);

            let user_root = tmp.path().join("user");
            let trust_dir = user_root.join(".ai/config/keys/trusted");
            fs::create_dir_all(&trust_dir).unwrap();

            ryeos_engine::trust::pin_key(
                &signing_key.verifying_key(),
                "test-publisher",
                &trust_dir,
                None,
            )
            .unwrap();

            Self {
                _tmp: tmp,
                source,
                ai_dir,
                signing_key,
                user_root,
            }
        }

        fn trust_store(&self) -> TrustStore {
            TrustStore::load_three_tier(None, Some(&self.user_root), &[]).unwrap()
        }

        fn add_kind_schema(&self, kind_name: &str) {
            let schema_dir = self.ai_dir.join("node/engine/kinds").join(kind_name);
            fs::create_dir_all(&schema_dir).unwrap();
            fs::write(
                schema_dir.join(format!("{kind_name}.kind-schema.yaml")),
                "kind: config\ndirectory: mykind\nextensions: []\n",
            )
            .unwrap();
        }

        fn write_signed_manifest(&self, body: &str) {
            let signed = lillux::signature::sign_content(body, &self.signing_key, "#", None);
            fs::write(self.ai_dir.join("manifest.yaml"), &signed).unwrap();
        }

        fn write_manifest_signed_by_other(&self, body: &str) {
            let other_key = SigningKey::generate(&mut OsRng);
            let signed = lillux::signature::sign_content(body, &other_key, "#", None);
            fs::write(self.ai_dir.join("manifest.yaml"), &signed).unwrap();
        }

        fn write_unsigned_manifest(&self, body: &str) {
            fs::write(self.ai_dir.join("manifest.yaml"), body).unwrap();
        }

        fn write_tampered_manifest(&self, original_body: &str) {
            let signed =
                lillux::signature::sign_content(original_body, &self.signing_key, "#", None);
            let tampered = signed.replace("version: '1.0'", "version: '9.9'");
            fs::write(self.ai_dir.join("manifest.yaml"), &tampered).unwrap();
        }
    }

    #[test]
    fn verify_manifest_accepts_valid_signed() {
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_signed_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds:\n  - mykind\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        assert!(
            verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).is_ok(),
            "valid signed manifest should pass"
        );
    }

    #[test]
    fn verify_manifest_rejects_unsigned() {
        let layout = BundleLayout::new("test-bundle");
        layout.write_unsigned_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no valid signature header"),
            "should reject unsigned: {msg}"
        );
    }

    #[test]
    fn verify_manifest_rejects_tampered() {
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_tampered_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds:\n  - mykind\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("signature verification failed"),
            "should reject tampered: {msg}"
        );
    }

    #[test]
    fn verify_manifest_rejects_untrusted_signer() {
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_manifest_signed_by_other(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds:\n  - mykind\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not in operator trust store"),
            "should reject untrusted signer: {msg}"
        );
    }

    #[test]
    fn verify_manifest_rejects_identity_mismatch() {
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_signed_manifest(
            "name: wrong-name\nversion: '1.0'\nprovides_kinds:\n  - mykind\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("identity mismatch"),
            "should reject name mismatch: {msg}"
        );
        assert!(
            msg.contains("test-bundle") && msg.contains("wrong-name"),
            "should name both sides: {msg}"
        );
    }

    #[test]
    fn verify_manifest_rejects_provides_kinds_mismatch() {
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_signed_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds:\n  - fake-kind\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("provides_kinds mismatch"),
            "should reject provides_kinds mismatch: {msg}"
        );
        assert!(
            msg.contains("fake-kind"),
            "should mention claimed kind: {msg}"
        );
    }

    #[test]
    fn verify_manifest_passes_without_manifest() {
        let layout = BundleLayout::new("test-bundle");
        let ts = layout.trust_store();
        assert!(
            verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).is_ok(),
            "no manifest should pass (optional)"
        );
    }

    // ── Slice 3: Instance validation wiring tests ──────────────────

    /// Helper: build a minimal `ValueShape` from YAML for tests.
    fn shape_from_yaml(yaml: &str) -> ryeos_engine::contracts::ValueShape {
        serde_yaml::from_str(yaml).expect("test contract YAML must parse")
    }

    /// Helper: simulate the preflight identity-composer validation path.
    fn validate_identity_composed_item(
        kind_schema: &ryeos_engine::kind_registry::KindSchema,
        parsed: &serde_json::Value,
    ) -> (Vec<String>, Vec<String>) {
        if kind_schema.composer != "handler:ryeos/core/identity" {
            return (vec![], vec![]);
        }
        let report = kind_schema
            .composed_value_contract
            .validate_instance(parsed);
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        for v in &report.errors {
            errors.push(format!(
                "contract violation [{}] {}: expected {}, found {}",
                v.code, v.path, v.expected, v.found,
            ));
        }
        for v in &report.warnings {
            warnings.push(format!(
                "contract warning [{}] {}: expected {}, found {}",
                v.code, v.path, v.expected, v.found,
            ));
        }
        (errors, warnings)
    }

    /// Helper: build a minimal KindSchema for identity-composer tests.
    fn identity_kind_schema(contract_yaml: &str) -> ryeos_engine::kind_registry::KindSchema {
        ryeos_engine::kind_registry::KindSchema {
            directory: "tools".to_string(),
            extensions: vec![],
            extraction_rules: std::collections::HashMap::new(),
            resolution: vec![],
            effective_trust: ryeos_engine::kind_registry::EffectiveTrustPolicy {
                include_references: false,
            },
            execution: None,
            composed_value_contract: shape_from_yaml(contract_yaml),
            composer: "handler:ryeos/core/identity".to_string(),
            composer_config: serde_json::Value::Null,
            runtime: None,
            inventory_kinds: vec![],
            inventory_schema_keys: vec![],
        }
    }

    /// Helper: build a minimal KindSchema for non-identity (extends) composer.
    fn extends_kind_schema(contract_yaml: &str) -> ryeos_engine::kind_registry::KindSchema {
        let mut schema = identity_kind_schema(contract_yaml);
        schema.composer = "handler:ryeos/core/extends-chain".to_string();
        schema
    }

    #[test]
    fn identity_descriptor_with_invalid_enum_produces_error() {
        let kind_schema = identity_kind_schema(
            r#"root_type: mapping
required:
  mode:
    type: single
    prim: string
    enum:
      - cli_exec
      - daemon_ui
optional: {}
"#,
        );

        let value = serde_json::json!({ "mode": "web_server" });
        let (errors, warnings) = validate_identity_composed_item(&kind_schema, &value);

        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("enum_mismatch"), "error: {}", errors[0]);
        assert!(errors[0].contains("mode"), "path in error: {}", errors[0]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn identity_descriptor_missing_nested_required_produces_error() {
        let kind_schema = identity_kind_schema(
            r#"root_type: mapping
required:
  launch:
    type: single
    prim: mapping
    contract:
      root_type: mapping
      required:
        mode:
          type: single
          prim: string
          enum:
            - cli_exec
            - daemon_ui
      optional: {}
optional: {}
"#,
        );

        let value = serde_json::json!({ "launch": {} });
        let (errors, warnings) = validate_identity_composed_item(&kind_schema, &value);

        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("missing_required_field"),
            "error: {}",
            errors[0]
        );
        assert!(
            errors[0].contains("launch.mode"),
            "dotted path in error: {}",
            errors[0]
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn non_identity_composer_skips_validation() {
        let kind_schema = extends_kind_schema(
            r#"root_type: mapping
required:
  mode:
    type: single
    prim: string
optional: {}
"#,
        );

        // Value is missing the required field, but since the composer
        // is NOT identity, validation should be skipped entirely.
        let value = serde_json::json!({ "other": "stuff" });
        let (errors, warnings) = validate_identity_composed_item(&kind_schema, &value);

        assert!(errors.is_empty(), "should skip validation for non-identity composer");
        assert!(warnings.is_empty());
    }

    #[test]
    fn unexpected_field_produces_warning_with_strict_warn() {
        let kind_schema = identity_kind_schema(
            r#"root_type: mapping
required:
  body:
    type: single
    prim: string
optional: {}
strict_fields: warn
"#,
        );

        let value = serde_json::json!({ "body": "hello", "extra": "field" });
        let (errors, warnings) = validate_identity_composed_item(&kind_schema, &value);

        assert!(errors.is_empty(), "unknown field should be warning, not error");
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("unexpected_field"),
            "warning code: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("extra"),
            "path in warning: {}",
            warnings[0]
        );
    }

    #[test]
    fn valid_identity_descriptor_passes() {
        let kind_schema = identity_kind_schema(
            r#"root_type: mapping
required:
  mode:
    type: single
    prim: string
    enum:
      - cli_exec
      - daemon_ui
optional:
  timeout:
    type: single
    prim: integer
strict_fields: warn
"#,
        );

        let value = serde_json::json!({ "mode": "cli_exec", "timeout": 30 });
        let (errors, warnings) = validate_identity_composed_item(&kind_schema, &value);

        assert!(errors.is_empty(), "should pass: {:?}", errors);
        assert!(warnings.is_empty(), "no warnings: {:?}", warnings);
    }

    #[test]
    fn preflight_issue_types_are_constructible() {
        // Verify the public types can be constructed for future use
        // by the output layer.
        let issue = PreflightIssue {
            item_path: "tools/my_tool.py".to_string(),
            severity: PreflightIssueSeverity::Error,
            code: InstanceViolationCode::EnumMismatch,
            path: "launch.mode".to_string(),
            expected: "cli_exec, daemon_ui".to_string(),
            found: "web_server".to_string(),
        };
        assert_eq!(issue.severity, PreflightIssueSeverity::Error);
        assert_eq!(issue.code, InstanceViolationCode::EnumMismatch);
        assert_eq!(issue.path, "launch.mode");

        let warning = PreflightIssue {
            item_path: "tools/my_tool.py".to_string(),
            severity: PreflightIssueSeverity::Warning,
            code: InstanceViolationCode::UnexpectedField,
            path: "extra".to_string(),
            expected: "known field".to_string(),
            found: "unknown field \"extra\"".to_string(),
        };
        assert_eq!(warning.severity, PreflightIssueSeverity::Warning);
    }
}
