//! Publisher-side `ryeos publish <bundle-source>` orchestration.
//!
//! Runs the full publish dance against a bundle source tree:
//!
//!   Phase 0:  Clean derived artifacts + strip stale signatures.
//!   Phase 1:  Bootstrap-sign kind schemas + parser/handler descriptors.
//!             Cuts the chicken-and-egg — these must be signed before the
//!             engine can load registries for Phase 3.
//!   Phase 2:  Rebuild CAS manifest (objects, refs, item_source sidecars).
//!   Phase 3:  Sign every other signable item (full engine validation).
//!   Phase 4:  Generate + sign bundle manifest (.ai/manifest.yaml).
//!             Reads manifest.source.yaml, derives provides_kinds from
//!             actual kind schemas, writes signed manifest.yaml.
//!   Phase 5:  Emit publisher trust doc (PUBLISHER_TRUST.toml).
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
use crate::actions::init::{materialize_manifest, BundleManifestSource};
use crate::actions::sign_bundle::{self, SignBundleReport};

#[derive(Debug)]
pub struct PublishOptions {
    /// Bundle source root (the directory containing `.ai/`).
    pub bundle_source: PathBuf,
    /// Registry root supplying kind schemas + parsers for sign-items.
    /// When publishing `core` itself, pass the same path as `bundle_source`.
    pub registry_root: PathBuf,
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
    pub bootstrap_kind_schemas: Vec<String>,
    pub bootstrap_parsers: Vec<String>,
    pub sign_report: SignBundleReport,
    pub rebuild_report: RebuildReport,
    /// Path to the generated + signed `.ai/manifest.yaml`, if a
    /// `manifest.source.yaml` was found. `None` for bundles without
    /// a source manifest (manifests are optional for third-party bundles).
    pub manifest_generated: Option<PathBuf>,
    pub publisher_trust_doc: Option<PathBuf>,
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
        bail!(
            "bundle_source has no .ai/ at {}",
            ai_dir.display()
        );
    }

    // ── Phase 0: clean derived artifacts from prior publish runs ──
    clean_derived_cas(&opts.bundle_source)?;
    // Strip stale signatures so registries load cleanly after bootstrap.
    strip_all_signatures(&ai_dir)?;

    // ── Phase 1: bootstrap-sign kind schemas + parser + handler descriptors ──
    // No registry or CAS dependency — signs YAML files in place using only
    // the signing key. This is the cycle-breaker.
    let (kind_schemas_signed, parsers_signed) =
        bootstrap_sign_kinds_and_parsers(&opts.bundle_source, &opts.signing_key)?;

    // ── Phase 2: rebuild CAS manifest ──
    // Only needs on-disk binaries under .ai/bin/<triple>/ + signing key.
    // Produces objects/, refs/bundles/manifest, and *.item_source.json
    // sidecars. No parser registry needed.
    let rebuild_report =
        build_bundle::rebuild_bundle_manifest(&opts.bundle_source, &opts.signing_key)
            .context("rebuild-manifest phase failed")?;

    // ── Phase 3: sign every other signable item ──
    // CAS now exists, HandlerRegistry can resolve binaries, parser
    // dispatcher works, validation runs, items get signed.
    let sign_report = sign_bundle::sign_bundle_items(
        &opts.bundle_source,
        &opts.registry_root,
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

    // ── Phase 4: generate + sign bundle manifest ──
    // Reads .ai/manifest.source.yaml (hand-authored), derives provides_kinds
    // from actual kind schemas on disk, writes .ai/manifest.yaml with a
    // signed envelope. Skipped silently when no source manifest exists
    // (manifests are optional for third-party bundles, required for official).
    let manifest_generated = generate_and_sign_manifest(
        &ai_dir,
        &opts.bundle_source,
        &opts.signing_key,
    )
    .context("manifest generation phase failed")?;

    // ── Phase 5: emit publisher trust doc ──
    let publisher_trust_doc = if opts.emit_trust_doc {
        Some(write_publisher_trust_doc(
            &opts.bundle_source,
            &opts.signing_key,
            &opts.owner,
        )?)
    } else {
        None
    };

    Ok(PublishReport {
        bundle_source: opts.bundle_source.clone(),
        author_fingerprint: rebuild_report.signer_fingerprint.clone(),
        bootstrap_kind_schemas: kind_schemas_signed,
        bootstrap_parsers: parsers_signed,
        sign_report,
        rebuild_report,
        manifest_generated,
        publisher_trust_doc,
    })
}

/// Sign every `*.kind-schema.yaml` under `<source>/.ai/node/engine/kinds/`,
/// every `*.yaml` under `<source>/.ai/parsers/`, and every `*.yaml` under
/// `<source>/.ai/handlers/` raw (no engine load).
///
/// These must be signed before Phase 2 (`sign_bundle_items`) because the
/// engine's `KindRegistry`, `ParserRegistry`, and `HandlerRegistry` all
/// verify signatures on load. Without bootstrap signing, Phase 2 cannot
/// construct the registries needed for metadata validation.
///
/// Skipped silently if directories don't exist.
fn bootstrap_sign_kinds_and_parsers(
    source: &Path,
    signing_key: &SigningKey,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut kind_schemas = Vec::new();
    let mut parsers = Vec::new();

    let kinds_dir = source.join(ryeos_engine::AI_DIR).join("node").join("engine").join("kinds");
    if kinds_dir.is_dir() {
        let mut files = Vec::new();
        collect_kind_schema_files(&kinds_dir, &mut files);
        for file in files {
            sign_raw_in_place(&file, signing_key, "#", None)
                .with_context(|| format!("bootstrap-sign {}", file.display()))?;
            kind_schemas.push(file.display().to_string());
        }
    }

    let parsers_dir = source.join(ryeos_engine::AI_DIR).join("parsers");
    if parsers_dir.is_dir() {
        let mut files = Vec::new();
        collect_yaml_files_recursive(&parsers_dir, &mut files);
        for file in files {
            sign_raw_in_place(&file, signing_key, "#", None)
                .with_context(|| format!("bootstrap-sign {}", file.display()))?;
            parsers.push(file.display().to_string());
        }
    }

    // Handler descriptors — must be signed before HandlerRegistry::load_base.
    let handlers_dir = source.join(ryeos_engine::AI_DIR).join("handlers");
    if handlers_dir.is_dir() {
        let mut files = Vec::new();
        collect_yaml_files_recursive(&handlers_dir, &mut files);
        for file in files {
            sign_raw_in_place(&file, signing_key, "#", None)
                .with_context(|| format!("bootstrap-sign {}", file.display()))?;
            parsers.push(file.display().to_string());
        }
    }

    Ok((kind_schemas, parsers))
}

fn sign_raw_in_place(
    path: &Path,
    signing_key: &SigningKey,
    prefix: &str,
    suffix: Option<&str>,
) -> Result<()> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let stripped = lillux::signature::strip_signature_lines(&content);
    let signed = lillux::signature::sign_content(&stripped, signing_key, prefix, suffix);
    let tmp = path.with_extension(format!("publish.tmp.{}", std::process::id()));
    fs::write(&tmp, &signed).with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Atomic write: stage to a temp file, then rename over the target.
/// Ensures readers never see a partially-written file.
fn atomic_write_str(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, content.as_bytes())
        .with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn collect_kind_schema_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
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
    let Ok(entries) = fs::read_dir(dir) else { return };
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

/// Remove derived artifacts from a prior publish run.
///
/// Called as part of Phase 0 (before any signing or manifest generation)
/// so that every phase works from a clean slate.
///
/// Cleans:
///   - `<bundle_source>/.ai/objects/`  (CAS blob store)
///   - `<bundle_source>/.ai/refs/`     (manifest ref pointers)
///   - `<bundle_source>/.ai/manifest.yaml` (generated + signed manifest)
///   - `**/*.item_source.json` under `.ai/bin/` (signed sidecars)
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
        fs::remove_dir_all(&refs_dir)
            .with_context(|| format!("remove {}", refs_dir.display()))?;
    }

    // Generated bundle manifest (from prior publish run)
    let generated_manifest = ai_dir.join("manifest.yaml");
    if generated_manifest.is_file() {
        fs::remove_file(&generated_manifest)
            .with_context(|| format!("remove {}", generated_manifest.display()))?;
    }

    // Per-triple MANIFEST.json + *.item_source.json sidecars
    let bin_root = ai_dir.join("bin");
    if bin_root.is_dir() {
        clean_bin_sidecars(&bin_root)?;
    }

    Ok(())
}

fn clean_bin_sidecars(bin_root: &Path) -> Result<()> {
    let entries = fs::read_dir(bin_root)
        .with_context(|| format!("read {}", bin_root.display()))?;
    for entry in entries.flatten() {
        let triple_dir = entry.path();
        if !triple_dir.is_dir() {
            continue;
        }
        let files = fs::read_dir(&triple_dir)
            .with_context(|| format!("read {}", triple_dir.display()))?;
        for file in files.flatten() {
            let p = file.path();
            if !p.is_file() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "MANIFEST.json" || name.ends_with(".item_source.json") {
                fs::remove_file(&p)
                    .with_context(|| format!("remove {}", p.display()))?;
            }
        }
    }
    Ok(())
}

/// Strip signature envelope lines from all signable files under a directory
/// tree. Handles YAML files (`.yaml`/`.yml`) and markdown directives (`.md`).
/// This prepares files for re-signing by removing stale signatures that would
/// cause verification failures during registry loads.
fn strip_all_signatures(dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let entries = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            strip_all_signatures(&p)?;
        } else if p.is_file() {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !matches!(ext, "yaml" | "yml" | "md") {
                continue;
            }
            let content = match fs::read_to_string(&p) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let stripped = lillux::signature::strip_signature_lines(&content);
            if stripped != content {
                let tmp = p.with_extension(format!("strip.tmp.{}", std::process::id()));
                fs::write(&tmp, &stripped)
                    .with_context(|| format!("write {}", tmp.display()))?;
                fs::rename(&tmp, &p)
                    .with_context(|| format!("rename {} -> {}", tmp.display(), p.display()))?;
            }
        }
    }
    Ok(())
}

/// Generate and sign the bundle manifest (Phase 4).
///
/// Reads `.ai/manifest.source.yaml` (hand-authored by the bundle author),
/// derives `provides_kinds` from actual kind schemas on disk, materializes
/// the full manifest, and writes it as `.ai/manifest.yaml` with a
/// `# ryeos:signed:...` envelope.
///
/// Returns `Some(path)` to the generated manifest, or `None` if no
/// `manifest.source.yaml` exists (manifests are optional for third-party
/// bundles — official bundles should always have one).
fn generate_and_sign_manifest(
    ai_dir: &Path,
    bundle_source: &Path,
    signing_key: &SigningKey,
) -> Result<Option<PathBuf>> {
    let source_path = ai_dir.join("manifest.source.yaml");
    if !source_path.exists() {
        tracing::info!(
            "no manifest.source.yaml — skipping manifest generation (optional for third-party bundles)"
        );
        return Ok(None);
    }

    // Derive bundle name from directory name (matches init.rs convention).
    let bundle_name = bundle_source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("bundle_source path has no directory name"))?;

    let raw = fs::read_to_string(&source_path)
        .with_context(|| format!("read manifest source {}", source_path.display()))?;
    let src: BundleManifestSource = serde_yaml::from_str(&raw)
        .with_context(|| format!("parse manifest source {}", source_path.display()))?;

    // Materialize: validates identity, derives provides_kinds from disk.
    let manifest = materialize_manifest(src, ai_dir, bundle_name)
        .context("materialize bundle manifest")?;

    let body = serde_yaml::to_string(&manifest)
        .context("serialize bundle manifest")?;
    let signed = lillux::signature::sign_content(&body, signing_key, "#", None);

    let target = ai_dir.join("manifest.yaml");
    atomic_write_str(&target, &signed)?;

    tracing::info!(
        path = %target.display(),
        name = %manifest.name,
        provides = ?manifest.provides_kinds,
        "generated + signed bundle manifest"
    );

    Ok(Some(target))
}

/// Write `<bundle_source>/PUBLISHER_TRUST.toml` — the universal trust
/// artifact downstream operators pin via `ryeos trust pin --from` or
/// `ryeos init --trust-file`.
fn write_publisher_trust_doc(
    bundle_source: &Path,
    signing_key: &SigningKey,
    owner: &str,
) -> Result<PathBuf> {
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
    let tmp = target.with_extension("tmp");
    fs::write(&tmp, body.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &target)
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
    Ok(target)
}
