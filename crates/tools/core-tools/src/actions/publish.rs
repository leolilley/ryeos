//! Publisher-side `ryeos publish <bundle-source>` orchestration.
//!
//! Runs the full publish dance against a bundle source tree:
//!
//!   Phase 0:  Clean derived CAS artifacts (objects, refs, sidecars).
//!   Phase 1:  Bootstrap-sign kind schemas + parser/handler descriptors.
//!             Idempotent: skips files already validly signed.
//!   Phase 2:  Rebuild CAS manifest (objects, refs, item_source sidecars)
//!             when the bundle owns `.ai/bin` binaries.
//!   Phase 3:  Sign every other signable item (full engine validation).
//!             Idempotent: validates existing signatures, re-signs only when needed.
//!   Phase 4:  Generate + sign bundle manifest (.ai/manifest.yaml).
//!             Idempotent: skips write when existing signed manifest matches.
//!   Phase 5:  Emit publisher trust doc (PUBLISHER_TRUST.toml).
//!             Idempotent: skips write when existing doc matches.
//!
//! Operates entirely on a publisher-provided source tree + signing key.
//! No daemon, no operator state, no ambient trust assumptions.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use lillux::crypto::SigningKey;
use serde::Serialize;

use crate::actions::build_bundle::{self, RebuildReport};
use crate::actions::sign_bundle::{self, SignBundleReport};
use ryeos_bundle::manifest::{materialize_manifest, BundleManifestSource};

#[derive(Debug)]
pub struct PublishOptions {
    /// Bundle source root (the directory containing `.ai/`).
    pub bundle_source: PathBuf,
    /// Registry/dependency roots supplying kind schemas + parsers for sign-items.
    ///
    /// When publishing `core` itself, pass the same path as `bundle_source`.
    /// Bundles that depend on kinds from multiple bundles (for example cockpit
    /// depends on `surface` from standard and base parsers/handlers from core)
    /// must pass each dependency root so every signable item is discovered and
    /// validated during authoring.
    pub registry_roots: Vec<PathBuf>,
    /// Author signing key used for every signing operation in this run.
    pub signing_key: SigningKey,
    /// Owner label written into PUBLISHER_TRUST.toml (e.g. "ryeos-official",
    /// "ryeos-dev"). Required when `emit_trust_doc` is true.
    pub owner: String,
    /// If `true`, write `<bundle_source>/PUBLISHER_TRUST.toml` summarizing
    /// the author key fingerprint + raw public key bytes for downstream
    /// operators to pin via `ryeos trust pin`. Default `true`.
    pub emit_trust_doc: bool,
}

#[derive(Debug, Serialize)]
pub struct PublishReport {
    pub bundle_source: PathBuf,
    pub author_fingerprint: String,
    pub bootstrap_validated: usize,
    pub bootstrap_signed: usize,
    pub sign_report: SignBundleReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebuild_report: Option<RebuildReport>,
    pub binary_rebuild_skipped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_rebuild_skip_reason: Option<String>,
    /// Path to the generated + signed `.ai/manifest.yaml`.
    pub manifest_generated: Option<PathBuf>,
    /// Whether the manifest was actually rewritten.
    pub manifest_changed: bool,
    /// Path to the emitted `PUBLISHER_TRUST.toml`.
    pub publisher_trust_doc: Option<PathBuf>,
    /// Whether the trust doc was actually rewritten.
    pub publisher_trust_doc_changed: bool,
}

pub fn run_publish(opts: &PublishOptions) -> Result<PublishReport> {
    if !opts.bundle_source.is_dir() {
        bail!(
            "bundle_source is not a directory: {}",
            opts.bundle_source.display()
        );
    }
    let ai_dir = opts.bundle_source.join(ryeos_engine::AI_DIR);
    if !ai_dir.is_dir() {
        bail!("bundle_source has no .ai/ at {}", ai_dir.display());
    }

    // ── Phase 0: clean derived CAS artifacts ──
    // Removes CAS blobs, ref pointers, and binary sidecars. Does NOT strip
    // signatures or delete manifest.yaml — both are handled idempotently
    // by their respective signing phases.
    clean_derived_cas(&opts.bundle_source)?;

    // ── Phase 1: bootstrap-sign kind schemas + parser + handler descriptors ──
    // Idempotent: skips files already validly signed by the current key.
    let (bootstrap_validated, bootstrap_signed) =
        bootstrap_sign_kinds_and_parsers(&opts.bundle_source, &opts.signing_key)?;

    // ── Phase 2: rebuild CAS manifest ──
    let bin_root = ai_dir.join("bin");
    let (rebuild_report, binary_rebuild_skipped, binary_rebuild_skip_reason) = if bin_root.is_dir()
    {
        (
            Some(
                build_bundle::rebuild_bundle_manifest(&opts.bundle_source, &opts.signing_key)
                    .context("rebuild-manifest phase failed")?,
            ),
            false,
            None,
        )
    } else {
        tracing::info!(
            path = %bin_root.display(),
            "no .ai/bin directory; skipping binary CAS manifest rebuild for declarative bundle"
        );
        (
            None,
            true,
            Some(format!(
                "no .ai/bin directory at {} — declarative bundle has no binary CAS manifest",
                bin_root.display()
            )),
        )
    };

    // ── Phase 3: sign every other signable item ──
    let sign_report = sign_bundle::sign_bundle_items(
        &opts.bundle_source,
        &opts.registry_roots,
        &opts.signing_key,
    )
    .context("sign-items phase failed")?;
    if !sign_report.is_total_success() {
        let mut msg = format!(
            "sign-items reported {} failure(s):\n",
            sign_report.failed.len()
        );
        for f in &sign_report.failed {
            msg.push_str(&format!(
                "  - {}: {}\n",
                f.item_ref,
                f.error.as_deref().unwrap_or("(no detail)")
            ));
        }
        bail!("{msg}");
    }

    // ── Phase 4: generate + sign bundle manifest (idempotent) ──
    let (manifest_generated, manifest_changed) =
        match generate_and_sign_manifest(&ai_dir, &opts.bundle_source, &opts.signing_key)
            .context("manifest generation phase failed")?
        {
            Some((path, changed)) => (Some(path), changed),
            None => (None, false),
        };

    // ── Phase 5: emit publisher trust doc (idempotent) ──
    let (publisher_trust_doc, publisher_trust_doc_changed) = if opts.emit_trust_doc {
        let result =
            write_publisher_trust_doc(&opts.bundle_source, &opts.signing_key, &opts.owner)?;
        (Some(result.0), result.1)
    } else {
        (None, false)
    };

    let author_fingerprint =
        lillux::signature::compute_fingerprint(&opts.signing_key.verifying_key());

    Ok(PublishReport {
        bundle_source: opts.bundle_source.clone(),
        author_fingerprint,
        bootstrap_validated,
        bootstrap_signed,
        sign_report,
        rebuild_report,
        binary_rebuild_skipped,
        binary_rebuild_skip_reason,
        manifest_generated,
        manifest_changed,
        publisher_trust_doc,
        publisher_trust_doc_changed,
    })
}

/// Bootstrap-sign kind schemas, parser descriptors, and handler descriptors.
///
/// Idempotent: checks each file's existing signature (hash + fingerprint +
/// signature validity) before writing. Only re-signs when the body changed,
/// the signer is wrong, or the signature is invalid.
///
/// Returns `(validated_count, signed_count)`.
fn bootstrap_sign_kinds_and_parsers(
    source: &Path,
    signing_key: &SigningKey,
) -> Result<(usize, usize)> {
    let mut validated = 0usize;
    let mut signed = 0usize;

    let kinds_dir = source
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("engine")
        .join("kinds");
    if kinds_dir.is_dir() {
        let mut files = Vec::new();
        collect_kind_schema_files(&kinds_dir, &mut files);
        for file in files {
            if sign_raw_in_place(&file, signing_key, "#", None)? {
                signed += 1;
            } else {
                validated += 1;
            }
        }
    }

    let parsers_dir = source.join(ryeos_engine::AI_DIR).join("parsers");
    if parsers_dir.is_dir() {
        let mut files = Vec::new();
        collect_yaml_files_recursive(&parsers_dir, &mut files);
        for file in files {
            if sign_raw_in_place(&file, signing_key, "#", None)? {
                signed += 1;
            } else {
                validated += 1;
            }
        }
    }

    let handlers_dir = source.join(ryeos_engine::AI_DIR).join("handlers");
    if handlers_dir.is_dir() {
        let mut files = Vec::new();
        collect_yaml_files_recursive(&handlers_dir, &mut files);
        for file in files {
            if sign_raw_in_place(&file, signing_key, "#", None)? {
                signed += 1;
            } else {
                validated += 1;
            }
        }
    }

    Ok((validated, signed))
}

/// Idempotent raw signer: checks existing signature before writing.
///
/// Returns `Ok(true)` if the file was (re-)signed, `Ok(false)` if the
/// existing signature was already valid and the file was left untouched.
fn sign_raw_in_place(
    path: &Path,
    signing_key: &SigningKey,
    prefix: &str,
    suffix: Option<&str>,
) -> Result<bool> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let stripped = lillux::signature::strip_signature_lines_with_envelope(&content, prefix, suffix);

    // Check if the file already has a valid signature for this body and key.
    if already_signed_for_body(&content, &stripped, signing_key, prefix, suffix) {
        return Ok(false);
    }

    let signed = lillux::signature::sign_content(&stripped, signing_key, prefix, suffix);
    let tmp = path.with_extension(format!("publish.tmp.{}", std::process::id()));
    fs::write(&tmp, &signed).with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(true)
}

/// Check whether `existing` (full file content) already carries a valid
/// signature for `body` (stripped content) signed by `signing_key`.
///
/// Returns true only when all three conditions hold:
///   1. Parsed header's content hash matches the body
///   2. Signer fingerprint matches the current key
///   3. Signature verifies against the hash
fn already_signed_for_body(
    existing: &str,
    body: &str,
    signing_key: &SigningKey,
    prefix: &str,
    suffix: Option<&str>,
) -> bool {
    let Some(first_line) = existing.lines().next() else {
        return false;
    };
    let Some(header) = lillux::signature::parse_signature_line(first_line, prefix, suffix) else {
        return false;
    };

    let verifying_key = signing_key.verifying_key();
    let fingerprint = lillux::signature::compute_fingerprint(&verifying_key);
    lillux::signature::is_valid_signature_for(
        &header.content_hash,
        &header.signature_b64,
        &header.signer_fingerprint,
        body,
        &verifying_key,
        &fingerprint,
    )
}

/// Atomic write: stage to a temp file, then rename over the target.
/// Ensures readers never see a partially-written file.
fn atomic_write_str(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, content.as_bytes()).with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn collect_kind_schema_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_kind_schema_files(&p, out);
        } else if p.is_file()
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".kind-schema.yaml"))
        {
            out.push(p);
        }
    }
}

fn collect_yaml_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_yaml_files_recursive(&p, out);
        } else if p.is_file()
            && matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("yaml") | Some("yml")
            )
        {
            out.push(p);
        }
    }
}

/// Remove derived CAS artifacts from a prior publish run.
///
/// Cleans:
///   - `<bundle_source>/.ai/objects/`  (CAS blob store)
///   - `<bundle_source>/.ai/refs/`     (manifest ref pointers)
///   - `**/*.item_source.json` under `.ai/bin/` (signed sidecars)
///
/// Does NOT strip signatures or delete manifest.yaml — those are
/// handled idempotently by their respective signing phases.
fn clean_derived_cas(bundle_source: &Path) -> Result<()> {
    let ai_dir = bundle_source.join(ryeos_engine::AI_DIR);

    // CAS objects
    let objects_dir = ai_dir.join("objects");
    if objects_dir.is_dir() {
        fs::remove_dir_all(&objects_dir)
            .with_context(|| format!("remove {}", objects_dir.display()))?;
    }

    // Ref pointers
    let refs_dir = ai_dir.join("refs");
    if refs_dir.is_dir() {
        fs::remove_dir_all(&refs_dir).with_context(|| format!("remove {}", refs_dir.display()))?;
    }

    // Per-triple MANIFEST.json + *.item_source.json sidecars
    let bin_root = ai_dir.join("bin");
    if bin_root.is_dir() {
        clean_bin_sidecars(&bin_root)?;
    }

    Ok(())
}

fn clean_bin_sidecars(bin_root: &Path) -> Result<()> {
    let entries = fs::read_dir(bin_root).with_context(|| format!("read {}", bin_root.display()))?;
    for entry in entries.flatten() {
        let triple_dir = entry.path();
        if !triple_dir.is_dir() {
            continue;
        }
        let files =
            fs::read_dir(&triple_dir).with_context(|| format!("read {}", triple_dir.display()))?;
        for file in files.flatten() {
            let p = file.path();
            if !p.is_file() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "MANIFEST.json" || name.ends_with(".item_source.json") {
                fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
            }
        }
    }
    Ok(())
}

/// Generate and sign the bundle manifest (Phase 4).
///
/// Idempotent: if the existing `.ai/manifest.yaml` already carries a valid
/// signature for the newly materialized body, no write occurs.
///
/// Returns `(Some((path, changed)))` where `changed` reflects whether
/// the file was actually written. Returns `None` if no `manifest.source.yaml`
/// exists (manifests are optional for third-party bundles).
fn generate_and_sign_manifest(
    ai_dir: &Path,
    bundle_source: &Path,
    signing_key: &SigningKey,
) -> Result<Option<(PathBuf, bool)>> {
    let source_path = ai_dir.join("manifest.source.yaml");
    if !source_path.exists() {
        // No source manifest — clean up any stale generated manifest.
        let target = ai_dir.join("manifest.yaml");
        if target.is_file() {
            fs::remove_file(&target)
                .with_context(|| format!("remove stale manifest {}", target.display()))?;
        }
        return Ok(None);
    }

    let bundle_name = bundle_source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("bundle_source path has no directory name"))?;

    let raw = fs::read_to_string(&source_path)
        .with_context(|| format!("read manifest source {}", source_path.display()))?;
    let src: BundleManifestSource = serde_yaml::from_str(&raw)
        .with_context(|| format!("parse manifest source {}", source_path.display()))?;

    let manifest =
        materialize_manifest(src, ai_dir, bundle_name).context("materialize bundle manifest")?;

    let body = serde_yaml::to_string(&manifest).context("serialize bundle manifest")?;
    let target = ai_dir.join("manifest.yaml");

    // Idempotent: skip write if existing signed manifest is already valid.
    if let Ok(existing) = fs::read_to_string(&target) {
        if already_signed_for_body(&existing, &body, signing_key, "#", None) {
            tracing::info!(
                path = %target.display(),
                name = %manifest.name,
                "manifest unchanged — skipping write"
            );
            return Ok(Some((target, false)));
        }
    }

    let signed = lillux::signature::sign_content(&body, signing_key, "#", None);
    atomic_write_str(&target, &signed)?;

    tracing::info!(
        path = %target.display(),
        name = %manifest.name,
        provides = ?manifest.provides_kinds,
        "generated + signed bundle manifest (changed)"
    );

    Ok(Some((target, true)))
}

/// Write `<bundle_source>/PUBLISHER_TRUST.toml` — the universal trust
/// artifact downstream operators pin via `ryeos trust pin --from` or
/// `ryeos init --trust-file`.
///
/// Idempotent: skips write when the existing file content matches.
///
/// Returns `(path, changed)`.
fn write_publisher_trust_doc(
    bundle_source: &Path,
    signing_key: &SigningKey,
    owner: &str,
) -> Result<(PathBuf, bool)> {
    let vk = signing_key.verifying_key();
    let fp = lillux::signature::compute_fingerprint(&vk);
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

    let doc = ryeos_engine::trust::PublisherTrustDoc {
        public_key: format!("ed25519:{key_b64}"),
        fingerprint: fp,
        owner: owner.to_string(),
    };

    let body = format!(
        "# Publisher trust pointer — pin with:\n\
         #     ryeos trust pin --from PUBLISHER_TRUST.toml\n\
         #     ryeos init --trust-file PUBLISHER_TRUST.toml\n\n\
         {}",
        doc.to_toml()
    );
    let target = bundle_source.join("PUBLISHER_TRUST.toml");

    // Idempotent: skip write when existing content matches.
    if let Ok(existing) = fs::read_to_string(&target) {
        if existing == body {
            return Ok((target, false));
        }
    }

    let tmp = target.with_extension(format!("trust-doc.tmp.{}", std::process::id()));
    fs::write(&tmp, body.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &target)
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
    Ok((target, true))
}
