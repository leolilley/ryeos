//! Publisher-side `rye publish <bundle-source>` orchestration.
//!
//! Runs the full publish dance against a bundle source tree:
//!
//!   1. Bootstrap-sign kind schemas + parser tools (cuts the chicken-and-egg).
//!      See `docs/SIGNING-CHICKEN-AND-EGG.md` for the full rationale.
//!   2. Sign every other signable item in the bundle (`sign_bundle_items`).
//!   3. Rebuild the CAS manifest (`rebuild_bundle_manifest`).
//!   4. Emit a publisher trust doc next to the bundle so downstream
//!      operators have a one-file pointer to pin via `rye trust pin`.
//!
//! Replaces the prior three-step author dance:
//! `bootstrap example` → `rye-bundle-tool sign-items` → `rebuild-manifest`.
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

#[derive(Debug)]
pub struct PublishOptions {
    /// Bundle source root (the directory containing `.ai/`).
    pub bundle_source: PathBuf,
    /// Registry root supplying kind schemas + parsers for sign-items.
    /// When publishing `core` itself, pass the same path as `bundle_source`.
    pub registry_root: PathBuf,
    /// Author signing key used for every signing operation in this run.
    pub signing_key: SigningKey,
    /// If `true`, write `<bundle_source>/PUBLISHER_TRUST.toml` summarizing
    /// the author key fingerprint + raw public key bytes for downstream
    /// operators to pin via `rye trust pin`. Default `true`.
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

    // ── Phase 1: bootstrap-sign kind schemas + parser tools ──
    let (kind_schemas_signed, parsers_signed) =
        bootstrap_sign_kinds_and_parsers(&opts.bundle_source, &opts.signing_key)?;

    // ── Phase 2: sign every other signable item ──
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

    // ── Phase 3: rebuild manifest ──
    let rebuild_report =
        build_bundle::rebuild_bundle_manifest(&opts.bundle_source, &opts.signing_key)
            .context("rebuild-manifest phase failed")?;

    // ── Phase 4: emit publisher trust doc ──
    let publisher_trust_doc = if opts.emit_trust_doc {
        Some(write_publisher_trust_doc(
            &opts.bundle_source,
            &opts.signing_key,
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
        publisher_trust_doc,
    })
}

/// Sign every `*.kind-schema.yaml` under `<source>/.ai/node/engine/kinds/`
/// and every `*.yaml` under `<source>/.ai/parsers/` raw (no engine load).
///
/// Mirrors `examples/bootstrap_sign_core_kind_schemas.rs` but as a library
/// function reusable by `run_publish`. Skipped silently if the directories
/// don't exist (bundles without their own kinds/parsers — only `core`
/// owns those today).
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

/// Write `<bundle_source>/PUBLISHER_TRUST.toml` — a one-file pointer
/// downstream operators can `rye trust pin` against. NOT a trust doc the
/// engine consumes; the engine's trust store is `~/.ai/config/keys/trusted/`
/// and is operator-pinned, never bundle-imported.
fn write_publisher_trust_doc(bundle_source: &Path, signing_key: &SigningKey) -> Result<PathBuf> {
    let vk = signing_key.verifying_key();
    let fp = lillux::signature::compute_fingerprint(&vk);
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
    let body = format!(
        r#"# Publisher trust pointer — NOT auto-imported by `bundle.install`.
# Downstream operators pin this fingerprint explicitly:
#
#     rye trust pin {fp} \
#         --pubkey-file PUBLISHER_TRUST.toml \
#         --owner "<publisher-name>"
#
# `rye trust pin` reads the `public_key` field below. This document is
# informational. The engine's trust store is operator-tier only
# ($USER/.ai/config/keys/trusted/).

fingerprint = "{fp}"
public_key  = "ed25519:{key_b64}"
"#
    );
    let target = bundle_source.join("PUBLISHER_TRUST.toml");
    let tmp = target.with_extension("tmp");
    fs::write(&tmp, body.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &target)
        .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
    Ok(target)
}
