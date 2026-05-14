//! Shared bundle-install preflight logic.
//!
//! `service:bundle/install` (in `ryeosd/src/services/handlers/bundle_install.rs`)
//! and the operator-side `ryeos init` standard-bundle path both call into
//! [`preflight_verify_bundle`] to enforce the trust contract:
//!
//! - All signable items in the bundle MUST be signed.
//! - The signer fingerprint MUST already be in the operator's trust store
//!   (loaded from project + user tier; system_space_dir and the bundle
//!   itself contribute ONLY kind schemas + parser tools, never trust docs).
//! - Path-anchoring validator MUST pass for every item.
//!
//! Parser descriptors are sourced from system_space_dir, the operator's
//! user tier, AND the bundle being verified — bundles MAY introduce new
//! parsers alongside new kinds. Trust still gates loading.
//!
//! Refusal modes:
//! - Untrusted signer → install rejected. The operator must
//!   `ryeos trust pin <fingerprint>` before retrying.
//! - Unsigned file under a kind directory → install rejected.
//! - Tampered content (hash mismatch) → install rejected.
//!
//! There is NO auto-import of trust docs from the bundle being installed.
//! Bundles do not ship trust docs in the published-bundle format any more;
//! see `docs/POST-KINDS-FLIP-PLAN.md` step 6.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use ryeos_engine::contracts::SignatureEnvelope;
use ryeos_engine::kind_registry::KindRegistry;
use std::sync::Arc;

use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::item_resolution::parse_signature_header;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

/// Verify every signable item in a bundle source tree.
///
/// `source_path`: bundle root to verify (the directory containing `.ai/`).
/// `system_space_dir`: where `core` lives — provides the kind schemas + parser
///   tools used to parse and validate `source_path` items. Does NOT
///   contribute trust docs.
/// `user_root`: parent of `~/.ai/`. Provides the operator's trust store
///   (`.ai/config/keys/trusted/`).
///
/// Returns `Ok(())` if every item parsed, validated, and verified against
/// the operator trust store. On any failure, returns an error listing every
/// failed item; install/copy is refused.
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

    // 1. Kind schemas come from system_space_dir + the bundle itself.
    //    The bundle's own kind schemas are loaded so its items can be
    //    parsed; this does not bypass trust because each kind schema is
    //    itself signature-verified via the loaded trust store.
    let mut schema_roots = Vec::new();
    let system_kinds = system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join(ryeos_engine::KIND_SCHEMAS_DIR);
    if system_kinds.is_dir() {
        schema_roots.push(system_kinds);
    }
    let bundle_kinds = ai_dir.join(ryeos_engine::KIND_SCHEMAS_DIR);
    if bundle_kinds.is_dir() {
        schema_roots.push(bundle_kinds.clone());
    }
    if schema_roots.is_empty() {
        bail!(
            "preflight: no kind schemas in system_space_dir ({}) or bundle ({})",
            system_space_dir.display(),
            bundle_kinds.display()
        );
    }

    // 2. Trust comes from operator tiers ONLY (project + user). The
    //    `system_roots` arg to `load_three_tier` is intentionally empty —
    //    bundle-internal `.ai/config/keys/trusted/` directories are NOT
    //    a trust source. Pin keys with `ryeos trust pin` instead.
    let trust_store = TrustStore::load_three_tier(None, user_root, &[])
        .context("preflight: load operator trust store")?;
    if trust_store.is_empty() {
        bail!(
            "preflight: operator trust store is empty — run `ryeos init` to \
             pin the platform author key, or `ryeos trust pin <fingerprint>` \
             to pin a third-party publisher"
        );
    }

    // 3. Load kind schemas (verified against trust store).
    let kinds = KindRegistry::load_base(&schema_roots, &trust_store)
        .context("preflight: load kind schemas")?;

    // 4. Load parser tools. Search roots: system, user, and the bundle
    //    being verified — bundles MAY ship their own parser descriptors
    //    needed for new kinds they introduce. Trust still gates loading.
    //
    //    Dedupe by canonicalized path: when `source_path == system_space_dir`
    //    (e.g. preflight verifying core in place during `ryeos init`) the
    //    same root must not be walked twice — `HandlerRegistry::load_base`
    //    rejects duplicate handler refs across roots.
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
    push_unique(system_space_dir.to_path_buf(), ryeos_engine::resolution::TrustClass::TrustedSystem, &mut parser_search_roots, &mut seen_roots);
    if let Some(ur) = user_root {
        push_unique(ur.to_path_buf(), ryeos_engine::resolution::TrustClass::TrustedUser, &mut parser_search_roots, &mut seen_roots);
    }
    push_unique(source_path.to_path_buf(), ryeos_engine::resolution::TrustClass::TrustedUser, &mut parser_search_roots, &mut seen_roots);

    // Diagnostic: warn if the source ships a legacy bundle-internal
    // trust dir, which the engine no longer treats as a trust source.
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

    // 5. Walk every signable file under each kind dir.
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

    // ── Manifest verification ──
    // If a generated manifest.yaml exists (from the publish pipeline),
    // verify its signature, identity, and provides_kinds consistency.
    // Manifests are optional — bundles without one pass preflight.
    verify_manifest_signature(&ai_dir, source_path, &trust_store)
        .context("preflight: bundle manifest verification")?;

    tracing::info!(
        source = %source_path.display(),
        "preflight verification passed"
    );
    Ok(())
}

/// Verify the generated bundle manifest (`.ai/manifest.yaml`), if present.
///
/// Checks:
/// 1. Signature header is valid and signer is trusted.
/// 2. Ed25519 signature verifies against the content hash.
/// 3. Manifest identity matches the bundle directory name.
/// 4. `provides_kinds` in the manifest matches actual kind schemas on disk.
///
/// Returns `Ok(())` if no manifest exists (manifests are optional for
/// third-party bundles), or if all checks pass.
fn verify_manifest_signature(
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

    // Signature envelope for YAML files: `# ryeos:signed:...`
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

    // Signer must be in operator trust store
    if !trust_store.is_trusted(&sig_header.signer_fingerprint) {
        bail!(
            "manifest.yaml: signer {} not in operator trust store \
             (run `ryeos trust pin {}` to trust this publisher)",
            sig_header.signer_fingerprint,
            sig_header.signer_fingerprint
        );
    }

    // Cryptographic signature verification (content hash + Ed25519)
    ryeos_engine::trust::verify_item_signature(&raw, &sig_header, &envelope, trust_store)
        .map_err(|e| anyhow::anyhow!("manifest.yaml signature verification failed: {e}"))?;

    // Parse manifest body (strip signature lines first)
    let body = lillux::signature::strip_signature_lines(&raw);
    let manifest: crate::actions::init::BundleManifest = serde_yaml::from_str(&body)
        .with_context(|| format!("parse manifest body from {}", manifest_path.display()))?;

    // Identity check: manifest name must match directory name
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

    // Provides_kinds consistency: what the manifest claims must match
    // actual kind schemas on disk (Guardrail 2.5 — normalize both sides).
    let actual_kinds = crate::actions::init::derive_provides_kinds(ai_dir)
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

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;

    /// Helper: create a temp bundle layout with `.ai/` and return paths.
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

            // Trust dir lives at <user_root>/.ai/config/keys/trusted/
            let user_root = tmp.path().join("user");
            let trust_dir = user_root.join(".ai/config/keys/trusted");
            fs::create_dir_all(&trust_dir).unwrap();

            // Pin the signing key into the trust dir
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

        /// Write kind schema so derive_provides_kinds returns something.
        fn add_kind_schema(&self, kind_name: &str) {
            let schema_dir = self.ai_dir.join("node/engine/kinds").join(kind_name);
            fs::create_dir_all(&schema_dir).unwrap();
            fs::write(
                schema_dir.join(format!("{kind_name}.kind-schema.yaml")),
                "kind: config\ndirectory: mykind\nextensions: []\n",
            )
            .unwrap();
        }

        /// Write a signed manifest.yaml with the given YAML body.
        fn write_signed_manifest(&self, body: &str) {
            let signed =
                lillux::signature::sign_content(body, &self.signing_key, "#", None);
            fs::write(self.ai_dir.join("manifest.yaml"), &signed).unwrap();
        }

        /// Write a signed manifest.yaml using a *different* key.
        fn write_manifest_signed_by_other(&self, body: &str) {
            let other_key = SigningKey::generate(&mut OsRng);
            let signed =
                lillux::signature::sign_content(body, &other_key, "#", None);
            fs::write(self.ai_dir.join("manifest.yaml"), &signed).unwrap();
        }

        /// Write a manifest.yaml without any signature.
        fn write_unsigned_manifest(&self, body: &str) {
            fs::write(self.ai_dir.join("manifest.yaml"), body).unwrap();
        }

        /// Write a signed manifest, then tamper with the body.
        fn write_tampered_manifest(&self, original_body: &str) {
            let signed =
                lillux::signature::sign_content(original_body, &self.signing_key, "#", None);
            // Replace a field value to break the content hash
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
        // Sign with a key that is NOT in the trust store
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
        // Manifest claims name "wrong-name" but directory is "test-bundle"
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
        // Manifest claims "fake-kind" but actual schemas only provide "mykind"
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
        // No manifest.yaml at all
        let ts = layout.trust_store();
        assert!(
            verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).is_ok(),
            "no manifest should pass (optional)"
        );
    }
}
