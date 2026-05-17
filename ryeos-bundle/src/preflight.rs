use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use std::sync::Arc;

use ryeos_engine::contracts::SignatureEnvelope;
use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::item_resolution::parse_signature_header;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

use crate::manifest::{derive_provides_kinds, BundleManifest};

pub fn preflight_verify_bundle(
    source_path: &Path,
    system_space_dir: &Path,
    user_root: Option<&Path>,
) -> Result<()> {
    let ai_dir = source_path.join(ryeos_engine::AI_DIR);
    if !ai_dir.is_dir() {
        bail!(
            "preflight: source has no .ai/ at {}",
            source_path.display()
        );
    }

    // ── Discover installed bundle roots (Model B) ──
    // Runtime uses registered bundle paths under .ai/bundles/*, NOT
    // system_space_dir itself. Preflight must mirror this so that
    // bundle-install / init can verify a bundle whose dependencies
    // are provided by a previously-installed bundle (e.g. standard
    // depends on core's parsers and kind schemas).
    let installed_bundle_roots = discover_installed_bundle_roots(system_space_dir);

    let mut schema_roots = Vec::new();
    for root in &installed_bundle_roots {
        let kinds_dir = root.join(ryeos_engine::AI_DIR).join(ryeos_engine::KIND_SCHEMAS_DIR);
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
    // Installed bundles are trusted system roots (mirrors daemon's Model B).
    for root in &installed_bundle_roots {
        push_unique(root.clone(), ryeos_engine::resolution::TrustClass::TrustedSystem, &mut parser_search_roots, &mut seen_roots);
    }
    if let Some(ur) = user_root {
        push_unique(ur.to_path_buf(), ryeos_engine::resolution::TrustClass::TrustedUser, &mut parser_search_roots, &mut seen_roots);
    }
    // Candidate bundle being verified (last, so installed content takes precedence).
    push_unique(source_path.to_path_buf(), ryeos_engine::resolution::TrustClass::TrustedUser, &mut parser_search_roots, &mut seen_roots);

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
    let (parser_tools, _dups) =
        ParserRegistry::load_base(&parser_search_roots.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(), &trust_store, &kinds)
            .context("preflight: load parser tools")?;
    let handler_registry = HandlerRegistry::load_base(&parser_search_roots, &trust_store)
        .context("preflight: load handler descriptors")?;
    let parser_dispatcher =
        ParserDispatcher::new(parser_tools, Arc::new(handler_registry));

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
            let ext = file_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
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

    let actual_kinds = derive_provides_kinds(ai_dir)
        .context("derive actual provides_kinds from kind schemas")?;
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
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Enumerate installed bundle roots under `system_space_dir/.ai/bundles/`.
/// Each immediate child directory that contains an `.ai/` subdirectory is
/// treated as an installed bundle root (mirrors the daemon's Model B).
/// Returns an empty Vec if the bundles directory doesn't exist or is empty —
/// this is valid for a clean first-bundle install.
fn discover_installed_bundle_roots(system_space_dir: &Path) -> Vec<PathBuf> {
    let bundles_dir = system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("bundles");

    let Ok(entries) = fs::read_dir(&bundles_dir) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_dir() && path.join(ryeos_engine::AI_DIR).is_dir() {
                Some(path)
            } else {
                None
            }
        })
        .collect()
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
            let signed =
                lillux::signature::sign_content(body, &self.signing_key, "#", None);
            fs::write(self.ai_dir.join("manifest.yaml"), &signed).unwrap();
        }

        fn write_manifest_signed_by_other(&self, body: &str) {
            let other_key = SigningKey::generate(&mut OsRng);
            let signed =
                lillux::signature::sign_content(body, &other_key, "#", None);
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
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts)
            .unwrap_err();
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
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts)
            .unwrap_err();
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
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts)
            .unwrap_err();
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
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts)
            .unwrap_err();
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
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts)
            .unwrap_err();
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
}
