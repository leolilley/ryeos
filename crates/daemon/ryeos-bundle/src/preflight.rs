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

const IDENTITY_COMPOSER: &str = "handler:ryeos/core/identity";

/// Severity of a preflight validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightIssueSeverity {
    /// Blocking — bundle verification fails.
    Error,
    /// Non-blocking — informational for authors.
    Warning,
}

/// A single validation issue found during bundle preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Warnings collected during a successful preflight verification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreflightReport {
    /// Non-blocking issues (e.g. unknown fields with strict_fields: warn).
    pub warnings: Vec<PreflightIssue>,
}

impl PreflightReport {
    /// Returns true if there are no warnings.
    pub fn is_clean(&self) -> bool {
        self.warnings.is_empty()
    }
}

/// Collect instance validation issues for identity-composed kinds.
///
/// Returns an empty vec for non-identity composers (extends-based kinds
/// are validated post-composition in the resolution pipeline).
fn collect_identity_contract_issues(
    item_rel: &Path,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    parsed: &serde_json::Value,
) -> Vec<PreflightIssue> {
    if kind_schema.composer != IDENTITY_COMPOSER {
        return Vec::new();
    }

    let item_path = item_rel.to_string_lossy().to_string();

    let report = kind_schema
        .composed_value_contract
        .validate_instance(parsed);

    let mut issues = Vec::with_capacity(report.errors.len() + report.warnings.len());
    for v in &report.errors {
        issues.push(PreflightIssue {
            item_path: item_path.clone(),
            severity: PreflightIssueSeverity::Error,
            code: v.code.clone(),
            path: v.path.clone(),
            expected: v.expected.clone(),
            found: v.found.clone(),
        });
    }
    for v in &report.warnings {
        issues.push(PreflightIssue {
            item_path: item_path.clone(),
            severity: PreflightIssueSeverity::Warning,
            code: v.code.clone(),
            path: v.path.clone(),
            expected: v.expected.clone(),
            found: v.found.clone(),
        });
    }
    issues
}

/// Format a preflight issue as a human-readable string.
fn format_preflight_issue(issue: &PreflightIssue) -> String {
    let label = match issue.severity {
        PreflightIssueSeverity::Error => "contract violation",
        PreflightIssueSeverity::Warning => "contract warning",
    };
    format!(
        "{}: {} [{}] {}: expected {}, found {}",
        issue.item_path, label, issue.code, issue.path, issue.expected, issue.found,
    )
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
    let _report =
        preflight_verify_bundle_report_in_context(source_path, dependency_bundle_roots, user_root)?;
    Ok(())
}

/// Like [`preflight_verify_bundle_in_context`] but returns a
/// [`PreflightReport`] containing non-blocking warnings on success.
///
/// CLI callers should use this to surface contract warnings to the user
/// without failing verification.
pub fn preflight_verify_bundle_report_in_context(
    source_path: &Path,
    dependency_bundle_roots: &[PathBuf],
    user_root: Option<&Path>,
) -> Result<PreflightReport> {
    // The core logic below populates `failures` (blocking) and
    // `warnings` (non-blocking). We lift the loop into this function
    // so both public APIs share a single implementation.
    preflight_verify_bundle_in_context_inner(source_path, dependency_bundle_roots, user_root)
}

/// Core preflight logic shared by both public entry points.
fn preflight_verify_bundle_in_context_inner(
    source_path: &Path,
    dependency_bundle_roots: &[PathBuf],
    user_root: Option<&Path>,
) -> Result<PreflightReport> {
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
    let mut warnings: Vec<PreflightIssue> = Vec::new();
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
            for issue in collect_identity_contract_issues(rel, kind_schema, &parsed) {
                match issue.severity {
                    PreflightIssueSeverity::Error => {
                        failures.push(format_preflight_issue(&issue));
                    }
                    PreflightIssueSeverity::Warning => {
                        warnings.push(issue);
                    }
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

    if !warnings.is_empty() {
        tracing::info!(
            source = %source_path.display(),
            warnings_count = warnings.len(),
            "preflight verification passed with warnings"
        );
    } else {
        tracing::info!(
            source = %source_path.display(),
            "preflight verification passed"
        );
    }
    Ok(PreflightReport { warnings })
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
    use std::os::unix::fs::PermissionsExt;

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

        fn sign_and_write(&self, rel: &str, body: &str) {
            let path = self.ai_dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let signed = lillux::signature::sign_content(body, &self.signing_key, "#", None);
            fs::write(path, signed).unwrap();
        }

        fn add_signed_kind_schema(&self, kind_name: &str, body: &str) {
            self.sign_and_write(
                &format!("node/engine/kinds/{kind_name}/{kind_name}.kind-schema.yaml"),
                body,
            );
        }

        fn add_test_parser_kind_schema(&self) {
            self.add_signed_kind_schema(
                "parser",
                &format!(
                    r##"location:
  directory: parsers
formats:
  - extensions: [".yaml", ".yml"]
    parser: parser:test/fixed/fixed
    signature:
      prefix: "#"
effective_trust:
  include_references: false
resolution: []
composer: {IDENTITY_COMPOSER}
composed_value_contract:
  root_type: mapping
  required: {{}}
"##
                ),
            );
        }

        fn add_test_item_kind_schema(&self, composer: &str, contract: &str) {
            self.add_signed_kind_schema(
                "mykind",
                &format!(
                    r##"location:
  directory: items
formats:
  - extensions: [".yaml", ".yml"]
    parser: parser:test/fixed/fixed
    signature:
      prefix: "#"
effective_trust:
  include_references: false
resolution: []
composer: {composer}
composed_value_contract:
{contract}
"##
                ),
            );
        }

        fn add_fixed_parser_descriptor(&self) {
            self.sign_and_write(
                "parsers/test/fixed/fixed.yaml",
                r#"version: "1.0.0"
description: "fixed parser for preflight tests"
handler: "handler:test/fixed-parser"
parser_api_version: 1
parser_config: {}
output_schema:
  root_type: mapping
  required: {}
"#,
            );
        }

        fn add_fixed_parser_handler(&self, value: serde_json::Value) {
            let triple = host_triple();
            let name = "fixed-parser";
            let bin_rel = format!("bin/{triple}/{name}");
            let bin_path = self.ai_dir.join(&bin_rel);
            fs::create_dir_all(bin_path.parent().unwrap()).unwrap();

            let response = serde_json::json!({
                "result": "parse_ok",
                "value": value,
            })
            .to_string();
            let script = format!("#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{response}'\n");
            fs::write(&bin_path, script.as_bytes()).unwrap();
            let mut perms = fs::metadata(&bin_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin_path, perms).unwrap();

            self.sign_and_write(
                "handlers/test/fixed-parser.yaml",
                &format!(
                    r#"category: test
name: fixed-parser
kind: handler
serves: parser
binary_ref: {bin_rel}
abi_version: "v1"
required_caps: []
description: "fixed parser handler for preflight tests"
"#
                ),
            );
            self.write_binary_manifest(&bin_rel, &bin_path);
        }

        fn write_binary_manifest(&self, item_ref: &str, bin_path: &Path) {
            let cas = lillux::cas::CasStore::new(self.ai_dir.join("objects"));
            let bytes = fs::read(bin_path).unwrap();
            let blob_hash = cas.store_blob(&bytes).unwrap();
            let fingerprint =
                ryeos_engine::trust::compute_fingerprint(&self.signing_key.verifying_key());
            let item_source = serde_json::json!({
                "item_ref": item_ref,
                "content_blob_hash": blob_hash,
                "integrity": format!("sha256:{blob_hash}"),
                "signature_info": { "fingerprint": fingerprint },
                "mode": 0o755,
            });
            let item_source_hash = cas.store_object(&item_source).unwrap();
            let manifest = serde_json::json!({
                "item_source_hashes": {
                    item_ref: item_source_hash,
                }
            });
            let manifest_hash = cas.store_object(&manifest).unwrap();
            let manifest_ref = self.ai_dir.join("refs/bundles/manifest");
            fs::create_dir_all(manifest_ref.parent().unwrap()).unwrap();
            fs::write(manifest_ref, manifest_hash).unwrap();
        }

        fn write_signed_item(&self, rel: &str, body: &str) {
            self.sign_and_write(rel, body);
        }
    }

    fn host_triple() -> String {
        let output = std::process::Command::new("rustc")
            .args(["-vV"])
            .output()
            .expect("rustc -vV");
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find(|l| l.starts_with("host:"))
            .expect("host triple in rustc -vV")
            .strip_prefix("host:")
            .unwrap()
            .trim()
            .to_string()
    }

    fn indented_contract(body: &str) -> String {
        body.lines()
            .map(|line| format!("  {line}\n"))
            .collect::<String>()
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

    fn add_real_preflight_fixture(
        layout: &BundleLayout,
        composer: &str,
        contract: &str,
        parsed_value: serde_json::Value,
    ) {
        layout.add_test_parser_kind_schema();
        layout.add_test_item_kind_schema(composer, &indented_contract(contract));
        layout.add_fixed_parser_descriptor();
        layout.add_fixed_parser_handler(parsed_value);
        layout.write_signed_item("items/demo.yaml", "name: demo\n");
    }

    #[test]
    fn preflight_report_real_wiring_rejects_identity_contract_error() {
        let layout = BundleLayout::new("test-bundle");
        add_real_preflight_fixture(
            &layout,
            IDENTITY_COMPOSER,
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
            serde_json::json!({ "mode": "web_server" }),
        );

        let err =
            preflight_verify_bundle_report_in_context(&layout.source, &[], Some(&layout.user_root))
                .unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains("contract violation [enum_mismatch]"),
            "error: {msg}"
        );
        assert!(msg.contains("items/demo.yaml"), "item path: {msg}");
        assert!(msg.contains("mode"), "field path: {msg}");
    }

    #[test]
    fn preflight_report_real_wiring_skips_non_identity_contract_error() {
        let layout = BundleLayout::new("test-bundle");
        add_real_preflight_fixture(
            &layout,
            "handler:ryeos/core/extends-chain",
            r#"root_type: mapping
required:
  mode:
    type: single
    prim: string
optional: {}
"#,
            serde_json::json!({ "other": "stuff" }),
        );

        let report =
            preflight_verify_bundle_report_in_context(&layout.source, &[], Some(&layout.user_root))
                .expect("non-identity composer should skip pre-composition contract validation");

        assert!(report.is_clean(), "no warnings expected: {report:?}");
    }

    #[test]
    fn preflight_report_real_wiring_returns_contract_warnings() {
        let layout = BundleLayout::new("test-bundle");
        add_real_preflight_fixture(
            &layout,
            IDENTITY_COMPOSER,
            r#"root_type: mapping
required:
  body:
    type: single
    prim: string
optional: {}
strict_fields: warn
"#,
            serde_json::json!({ "body": "hello", "extra": "field" }),
        );

        let report =
            preflight_verify_bundle_report_in_context(&layout.source, &[], Some(&layout.user_root))
                .expect("warnings should not fail preflight");

        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].severity, PreflightIssueSeverity::Warning);
        assert_eq!(
            report.warnings[0].code,
            InstanceViolationCode::UnexpectedField
        );
        assert_eq!(report.warnings[0].item_path, "items/demo.yaml");
        assert_eq!(report.warnings[0].path, "extra");
    }

    // ── Slice 3 follow-up: Contract diagnostics wiring tests ──────

    /// Helper: build a minimal `ValueShape` from YAML for tests.
    fn shape_from_yaml(yaml: &str) -> ryeos_engine::contracts::ValueShape {
        serde_yaml::from_str(yaml).expect("test contract YAML must parse")
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
            composer: IDENTITY_COMPOSER.to_string(),
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

    // ── Tests for collect_identity_contract_issues ────────────────

    #[test]
    fn collect_issues_returns_errors_for_invalid_enum() {
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
        let issues = collect_identity_contract_issues(
            &PathBuf::from("tools/my_tool.py"),
            &kind_schema,
            &value,
        );

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, PreflightIssueSeverity::Error);
        assert_eq!(issues[0].code, InstanceViolationCode::EnumMismatch);
        assert_eq!(issues[0].path, "mode");
        assert_eq!(issues[0].item_path, "tools/my_tool.py");
        assert!(issues[0].found.contains("web_server"));
    }

    #[test]
    fn collect_issues_returns_nested_dotted_path() {
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
        let issues = collect_identity_contract_issues(
            &PathBuf::from("tools/my_tool.py"),
            &kind_schema,
            &value,
        );

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, PreflightIssueSeverity::Error);
        assert_eq!(issues[0].code, InstanceViolationCode::MissingRequiredField);
        assert_eq!(issues[0].path, "launch.mode");
    }

    #[test]
    fn collect_issues_skips_non_identity_composer() {
        let kind_schema = extends_kind_schema(
            r#"root_type: mapping
required:
  mode:
    type: single
    prim: string
optional: {}
"#,
        );
        let value = serde_json::json!({ "other": "stuff" });
        let issues = collect_identity_contract_issues(
            &PathBuf::from("directives/my_directive.md"),
            &kind_schema,
            &value,
        );

        assert!(
            issues.is_empty(),
            "non-identity composer should produce no issues"
        );
    }

    #[test]
    fn collect_issues_returns_warnings_for_strict_fields() {
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
        let issues = collect_identity_contract_issues(
            &PathBuf::from("tools/my_tool.py"),
            &kind_schema,
            &value,
        );

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, PreflightIssueSeverity::Warning);
        assert_eq!(issues[0].code, InstanceViolationCode::UnexpectedField);
        assert_eq!(issues[0].path, "extra");
    }

    #[test]
    fn collect_issues_returns_nothing_for_valid_descriptor() {
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
        let issues = collect_identity_contract_issues(
            &PathBuf::from("tools/my_tool.py"),
            &kind_schema,
            &value,
        );

        assert!(
            issues.is_empty(),
            "valid descriptor should produce no issues"
        );
    }

    // ── Tests for format_preflight_issue ─────────────────────────────

    #[test]
    fn format_issue_includes_all_fields() {
        let issue = PreflightIssue {
            item_path: "tools/my_tool.py".to_string(),
            severity: PreflightIssueSeverity::Error,
            code: InstanceViolationCode::EnumMismatch,
            path: "launch.mode".to_string(),
            expected: "cli_exec, daemon_ui".to_string(),
            found: "web_server".to_string(),
        };
        let formatted = format_preflight_issue(&issue);
        assert!(
            formatted.contains("tools/my_tool.py"),
            "item_path: {formatted}"
        );
        assert!(
            formatted.contains("contract violation"),
            "label: {formatted}"
        );
        assert!(formatted.contains("enum_mismatch"), "code: {formatted}");
        assert!(formatted.contains("launch.mode"), "path: {formatted}");
        assert!(formatted.contains("cli_exec"), "expected: {formatted}");
        assert!(formatted.contains("web_server"), "found: {formatted}");
    }

    #[test]
    fn format_issue_uses_warning_label_for_warnings() {
        let issue = PreflightIssue {
            item_path: "tools/my_tool.py".to_string(),
            severity: PreflightIssueSeverity::Warning,
            code: InstanceViolationCode::UnexpectedField,
            path: "extra".to_string(),
            expected: "known field".to_string(),
            found: "string".to_string(),
        };
        let formatted = format_preflight_issue(&issue);
        assert!(
            formatted.contains("contract warning"),
            "should use warning label: {formatted}"
        );
        assert!(
            !formatted.contains("contract violation"),
            "should not use error label: {formatted}"
        );
    }

    // ── Tests for PreflightReport ────────────────────────────────────

    #[test]
    fn preflight_report_is_clean_when_empty() {
        let report = PreflightReport::default();
        assert!(report.is_clean());
    }

    #[test]
    fn preflight_report_is_dirty_with_warnings() {
        let report = PreflightReport {
            warnings: vec![PreflightIssue {
                item_path: "tools/x.py".to_string(),
                severity: PreflightIssueSeverity::Warning,
                code: InstanceViolationCode::UnexpectedField,
                path: "extra".to_string(),
                expected: "known field".to_string(),
                found: "string".to_string(),
            }],
        };
        assert!(!report.is_clean());
        assert_eq!(report.warnings.len(), 1);
    }

    // NOTE: Real preflight wiring tests (calling
    // `preflight_verify_bundle_in_context` directly with temp bundle
    // fixtures) require parser binaries installed in the worktree.
    // These will be validated in CI. The tests above exercise the
    // production helper functions (`collect_identity_contract_issues`,
    // `format_preflight_issue`) and public types (`PreflightIssue`,
    // `PreflightReport`) that the real wiring calls.
}
