//! Standalone bundle-manifest signing.
//!
//! Generates and signs `.ai/manifest.yaml` from `.ai/manifest.source.yaml`
//! without running the full publish pipeline (no CAS clean, no item signing,
//! no trust doc). It reuses the same idempotent materialize-and-sign step the
//! publish path's Phase 4 runs, so a manifest authored here is byte-identical
//! to one produced by a full publish.
//!
//! Use when iterating on a manifest's declarations (e.g. under
//! `runtime_authority:`) without re-signing every item in the tree.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lillux::crypto::SigningKey;
use serde::Serialize;

use crate::actions::publish::generate_and_sign_manifest_in_place;

#[derive(Debug, Serialize)]
pub struct ManifestSignReport {
    pub bundle_source: PathBuf,
    pub author_fingerprint: String,
    /// Path to the generated + signed `.ai/manifest.yaml`.
    pub manifest_generated: PathBuf,
    /// Whether the manifest file was actually (re)written.
    pub manifest_changed: bool,
}

/// Generate and sign `.ai/manifest.yaml` for the bundle at `bundle_source`.
///
/// `name` overrides the effective bundle id the manifest must carry — the
/// first bare-id segment of the bundle's item refs, which runtime authority
/// requires to equal `manifest.name`. `None` uses the source directory's
/// basename. The manifest source's declared `name` must equal this value or
/// materialization fails loudly.
pub fn manifest_sign(
    bundle_source: &Path,
    name: Option<&str>,
    signing_key: &SigningKey,
) -> Result<ManifestSignReport> {
    let live_root = bundle_source.to_path_buf();
    let canonical_live_root = std::fs::canonicalize(bundle_source).with_context(|| {
        format!(
            "canonicalize bundle source before manifest signing {}",
            bundle_source.display()
        )
    })?;
    let effective_name = match name {
        Some(name) => name.to_string(),
        None => canonical_live_root
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .context("bundle_source path has no UTF-8 directory name")?,
    };
    super::publisher_transaction::with_staged_bundle_generation(bundle_source, |staging| {
        let mut report = manifest_sign_in_place(staging, &effective_name, signing_key)?;
        report.bundle_source = live_root.clone();
        let live_manifest_path = report
            .manifest_generated
            .strip_prefix(staging)
            .map(|relative| live_root.join(relative))
            .unwrap_or_else(|_| report.manifest_generated.clone());
        report.manifest_generated = live_manifest_path;
        Ok(report)
    })
}

fn manifest_sign_in_place(
    bundle_source: &Path,
    effective_name: &str,
    signing_key: &SigningKey,
) -> Result<ManifestSignReport> {
    if !bundle_source.is_dir() {
        anyhow::bail!(
            "bundle_source is not a directory: {}",
            bundle_source.display()
        );
    }
    let ai_dir = bundle_source.join(ryeos_engine::AI_DIR);
    if !ai_dir.is_dir() {
        anyhow::bail!("bundle_source has no .ai/ at {}", ai_dir.display());
    }

    let (manifest_generated, manifest_changed) =
        generate_and_sign_manifest_in_place(bundle_source, effective_name, signing_key)
            .context("manifest generation failed")?;

    let author_fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());

    Ok(ManifestSignReport {
        bundle_source: bundle_source.to_path_buf(),
        author_fingerprint,
        manifest_generated,
        manifest_changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn generates_signed_manifest_without_full_publish() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("arc");
        let ai = bundle.join(ryeos_engine::AI_DIR);
        write(
            &ai.join("manifest.source.yaml"),
            "name: arc\nversion: \"0.1.0\"\ndescription: test agent\n",
        );

        let report = manifest_sign(&bundle, None, &test_key()).expect("manifest sign");
        let manifest_path = report.manifest_generated;
        assert_eq!(manifest_path, ai.join("manifest.yaml"));
        assert!(report.manifest_changed);

        let body = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(body.starts_with("# ryeos:signed:"), "body: {body}");
        assert!(body.contains("name: arc"));

        // Idempotent: a second run rewrites nothing.
        let again = manifest_sign(&bundle, None, &test_key()).expect("manifest sign 2");
        assert!(!again.manifest_changed);
    }

    #[test]
    fn name_override_is_the_effective_id_and_must_match_source() {
        let tmp = tempfile::tempdir().unwrap();
        // Directory basename (`arc-src`) deliberately differs from the
        // declared effective bundle id (`arc`).
        let bundle = tmp.path().join("arc-src");
        let ai = bundle.join(ryeos_engine::AI_DIR);
        write(
            &ai.join("manifest.source.yaml"),
            "name: arc\nversion: \"0.1.0\"\n",
        );

        // Without an override the basename is the expected name → mismatch.
        let err = manifest_sign(&bundle, None, &test_key()).unwrap_err();
        assert!(
            format!("{err:#}").contains("identity mismatch"),
            "got {err:#}"
        );

        // With the override equal to the declared name it materializes.
        let report = manifest_sign(&bundle, Some("arc"), &test_key()).expect("override");
        assert!(report.manifest_generated.is_file());

        // An override that disagrees with the declared name is rejected.
        let err = manifest_sign(&bundle, Some("nope"), &test_key()).unwrap_err();
        assert!(
            format!("{err:#}").contains("identity mismatch"),
            "got {err:#}"
        );
    }
}
