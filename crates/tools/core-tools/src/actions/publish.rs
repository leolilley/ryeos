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
use ryeos_engine::trust::TrustStore;

#[derive(Debug)]
pub struct PublishOptions {
    /// Bundle source root (the directory containing `.ai/`).
    pub bundle_source: PathBuf,
    /// Registry/dependency roots supplying kind schemas + parsers for sign-items.
    ///
    /// When publishing `core` itself, pass the same path as `bundle_source`.
    /// Bundles that depend on kinds from multiple bundles (for example Studio
    /// depends on `surface` from standard and base parsers/handlers from core)
    /// must pass each dependency root so every signable item is discovered and
    /// validated during authoring.
    pub registry_roots: Vec<PathBuf>,
    /// Author signing key used for every signing operation in this run.
    pub signing_key: SigningKey,
    /// Operator trust store used to verify dependency bundle schemas,
    /// parsers, and handlers during the sign-items phase.
    pub base_trust_store: Option<TrustStore>,
    /// Owner label written into PUBLISHER_TRUST.toml (e.g. "ryeos-official",
    /// "ryeos-dev"). Required when `emit_trust_doc` is true.
    pub owner: String,
    /// Effective bundle id the generated manifest must carry — the first
    /// bare-id segment of the bundle's item refs (runtime authority requires
    /// `manifest.name` to equal it). `None` falls back to the bundle source
    /// directory's basename.
    pub name: Option<String>,
    /// If `true`, items that fail to sign in Phase 3 are reported and skipped
    /// instead of aborting the publish — the run continues to manifest
    /// generation and the report is marked `partial`. The trust doc is
    /// suppressed so a partial publish never looks like a clean release.
    /// Default `false` (fail-fast).
    pub skip_unsignable: bool,
    /// If `true`, publish a bundle that declares runtime authority even when an
    /// item's effective bundle id diverges from the manifest name. By default
    /// such a mismatch is fatal, because the daemon hard-fails runtime-cap
    /// minting for it — a published-but-unusable manifest. Default `false`.
    pub allow_namespace_mismatch: bool,
    /// If `true`, do not fail when a populated `.ai/<dir>` is covered by no
    /// registered kind. Set this ONLY for a deliberately partial intermediate
    /// publish (e.g. signing core before the bundle defining its `knowledge`
    /// kind is available), which a later republish then completes. Default
    /// `false`: an uncovered item directory hard-fails the publish.
    pub allow_uncovered_item_dirs: bool,
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
    /// `true` when `skip_unsignable` swallowed one or more sign failures — the
    /// publish is incomplete. Never `true` on a clean fail-fast publish.
    pub partial: bool,
    /// Item refs that failed to sign and were skipped (only populated when
    /// `partial`). A clean publish leaves this empty.
    pub skipped_unsignable: Vec<String>,
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
    let mut sign_report = sign_bundle::sign_bundle_items_with_trust(
        &opts.bundle_source,
        &opts.registry_roots,
        &opts.signing_key,
        opts.base_trust_store.as_ref(),
        opts.allow_uncovered_item_dirs,
    )
    .context("sign-items phase failed")?;
    let mut partial = false;
    let mut skipped_unsignable: Vec<String> = Vec::new();
    if !sign_report.is_total_success() {
        if opts.skip_unsignable {
            partial = true;
            skipped_unsignable = sign_report
                .failed
                .iter()
                .map(|f| f.item_ref.clone())
                .collect();
            tracing::warn!(
                skipped = skipped_unsignable.len(),
                "skip-unsignable: PARTIAL publish — continuing past {} unsignable item(s); \
                 the manifest is still generated but this is NOT a clean release",
                skipped_unsignable.len()
            );
            for f in &sign_report.failed {
                tracing::warn!(
                    item = %f.item_ref,
                    error = %f.error.as_deref().unwrap_or("(no detail)"),
                    "skipped unsignable item"
                );
            }
        } else {
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
    }

    // ── Effective-bundle-id + config-shadow lint ──
    // Only meaningful when the bundle asserts an effective id the runtime
    // enforces (an explicit `--name`, or a manifest that declares runtime
    // authority) or declares config `shadows:` to verify. Free-form item
    // namespacing (e.g. core's `ryeos/...`) is left unlinted so it does not
    // produce noise.
    if let Some(ctx) = lint_context(&ai_dir, opts.name.as_deref())? {
        let lint = lint_item_namespaces(&sign_report, &ctx.expected, &ctx.shadows);
        for w in lint.warnings.iter().chain(&lint.shadow_warnings) {
            tracing::warn!(item = %w.item_ref, "{}", w.message);
        }
        for n in &lint.notes {
            tracing::info!(item = %n.item_ref, "{}", n.message);
        }
        // Only cap-minting divergence can escalate to a fatal publish error;
        // shadow-declaration mismatches are advisory. Count them before folding
        // the advisory warnings into the reported set.
        let cap_count = lint.warnings.len();
        sign_report.warnings = lint
            .warnings
            .into_iter()
            .chain(lint.shadow_warnings)
            .collect();
        sign_report.notes = lint.notes;
        // A cap-minting item under a divergent namespace is fatal for a bundle
        // that declares runtime authority: the daemon hard-fails cap minting at
        // runtime, so the manifest would publish but never work. Inert
        // cross-namespace items are notes, not warnings, and never trip this.
        if cap_count > 0
            && !opts.allow_namespace_mismatch
            && manifest_declares_runtime_authority(&ai_dir)?
        {
            bail!(
                "refusing to publish: {} cap-minting item(s) have an effective bundle id that \
                 diverges from '{}'. The daemon mints their runtime-authority caps under the \
                 wrong namespace and rejects them at dispatch, so the manifest would publish but \
                 never work. Namespace these items under '{}/…' (or set the bundle --name to \
                 match). Inert cross-namespace items (config shadows, knowledge) are reported as \
                 notes, not errors, and need no change. As a last resort, \
                 --allow-namespace-mismatch bypasses this check.",
                cap_count,
                ctx.expected,
                ctx.expected
            );
        }
    }

    // ── Phase 4: generate + sign bundle manifest (idempotent) ──
    let (manifest_generated, manifest_changed) = match generate_and_sign_manifest(
        &ai_dir,
        &opts.bundle_source,
        opts.name.as_deref(),
        &opts.signing_key,
    )
    .context("manifest generation phase failed")?
    {
        Some((path, changed)) => (Some(path), changed),
        None => (None, false),
    };

    // ── Phase 5: emit publisher trust doc (idempotent) ──
    // Suppressed on a partial publish: a trust doc is a clean-release artifact
    // and must not be emitted when items were skipped.
    let (publisher_trust_doc, publisher_trust_doc_changed) = if opts.emit_trust_doc && !partial {
        let result =
            write_publisher_trust_doc(&opts.bundle_source, &opts.signing_key, &opts.owner)?;
        (Some(result.0), result.1)
    } else {
        if opts.emit_trust_doc && partial {
            tracing::warn!(
                "skip-unsignable: suppressing PUBLISHER_TRUST.toml on a partial publish"
            );
        }
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
        partial,
        skipped_unsignable,
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
/// Inputs the namespace lint needs from the manifest source: the effective
/// bundle id items are checked against, and the declared config shadows their
/// observed foreign-namespace items are verified against.
struct LintContext {
    expected: String,
    shadows: Vec<String>,
}

/// Resolve the namespace-lint context, or `None` when the bundle should not be
/// linted.
///
/// Returns `Some` when `name_override` is set (the author explicitly asserts
/// the bundle id), when the manifest declares runtime authority (where the
/// effective bundle id is enforced when minting callback caps), or when it
/// declares config `shadows:` (whose declared-vs-observed check must run).
/// Otherwise `None` — item namespacing is free-form and must not be flagged.
fn lint_context(ai_dir: &Path, name_override: Option<&str>) -> Result<Option<LintContext>> {
    let source_path = ai_dir.join("manifest.source.yaml");
    let src = if source_path.exists() {
        let raw = fs::read_to_string(&source_path)
            .with_context(|| format!("read manifest source {}", source_path.display()))?;
        Some(
            serde_yaml::from_str::<BundleManifestSource>(&raw)
                .with_context(|| format!("parse manifest source {}", source_path.display()))?,
        )
    } else {
        None
    };
    let shadows = src.as_ref().map(|s| s.shadows.clone()).unwrap_or_default();

    let expected = match name_override {
        Some(name) => name.to_string(),
        None => match &src {
            Some(s) if !s.runtime_authority.is_empty() || !s.shadows.is_empty() => s.name.clone(),
            _ => return Ok(None),
        },
    };
    Ok(Some(LintContext { expected, shadows }))
}

/// True when the bundle's manifest source declares any runtime authority in any
/// family under `runtime_authority:`.
fn manifest_declares_runtime_authority(ai_dir: &Path) -> Result<bool> {
    let source_path = ai_dir.join("manifest.source.yaml");
    if !source_path.exists() {
        return Ok(false);
    }
    let raw = fs::read_to_string(&source_path)
        .with_context(|| format!("read manifest source {}", source_path.display()))?;
    let src: BundleManifestSource = serde_yaml::from_str(&raw)
        .with_context(|| format!("parse manifest source {}", source_path.display()))?;
    Ok(!src.runtime_authority.is_empty())
}

/// Effective bundle id of a signer-report ref `kind:bare_id` — the first
/// `/`-segment of the bare id, mirroring the daemon's
/// `effective_bundle_id_from_item_ref`. `None` when the ref carries no bare id.
fn item_effective_bundle_id(item_ref: &str) -> Option<&str> {
    let bare = item_ref.split_once(':').map(|(_, b)| b).unwrap_or(item_ref);
    bare.split('/').next().filter(|s| !s.is_empty())
}

/// Result of the effective-bundle-id lint, split by precision:
/// - `warnings` — cap-minting items (declaring manifest-backed runtime
///   authority) whose effective bundle id diverges. These are actionable: the
///   daemon would mint their caps under the wrong namespace and reject them at
///   dispatch. Only these can escalate to a fatal publish error.
/// - `shadow_warnings` — declared-vs-observed config-shadow mismatches: an
///   observed foreign-namespace config item with no matching `shadows:`
///   declaration (undeclared override), or a `shadows:` declaration with no
///   shipped item (stale intent). Advisory: surfaced as warnings, never fatal.
/// - `notes` — verified declared shadows and inert cross-namespace items
///   (e.g. knowledge) that declare no runtime authority and cannot mint caps.
///   Surfaced for visibility, never error-escalated.
struct NamespaceLint {
    warnings: Vec<sign_bundle::ItemWarning>,
    shadow_warnings: Vec<sign_bundle::ItemWarning>,
    notes: Vec<sign_bundle::ItemWarning>,
}

/// True when `item_ref` is a `config:` item — the kind project-first resolution
/// shadows, so a foreign-namespace config item is an override candidate.
fn is_config_ref(item_ref: &str) -> bool {
    item_ref
        .split_once(':')
        .is_some_and(|(kind, _)| kind == "config")
}

/// Classify every signed/validated/failed item whose effective bundle id
/// diverges from `expected`, and verify declared config shadows both ways.
///
/// The split is capability-based, not namespace-based: a foreign namespace only
/// matters for cap minting (`declares_runtime_authority`). A project-first
/// config shadow keeps its foreign namespace on purpose (that is how it shadows
/// by exact ref) and never reaches the mint path — but it is verified against
/// the bundle's signed `shadows:` intent: an observed shadow with no declaration
/// warns (undeclared override), and a declaration with no shipped item warns
/// (stale intent).
fn lint_item_namespaces(
    report: &SignBundleReport,
    expected: &str,
    shadows: &[String],
) -> NamespaceLint {
    let mut warnings: Vec<sign_bundle::ItemWarning> = Vec::new();
    let mut shadow_warnings: Vec<sign_bundle::ItemWarning> = Vec::new();
    let mut notes: Vec<sign_bundle::ItemWarning> = Vec::new();

    let declared: std::collections::BTreeSet<&str> = shadows.iter().map(String::as_str).collect();
    let mut matched: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

    for outcome in report
        .validated
        .iter()
        .chain(&report.signed)
        .chain(&report.failed)
    {
        let Some(eff) = item_effective_bundle_id(&outcome.item_ref) else {
            continue;
        };
        if eff == expected {
            continue;
        }
        if outcome.declares_runtime_authority {
            warnings.push(sign_bundle::ItemWarning {
                item_ref: outcome.item_ref.clone(),
                message: format!(
                    "effective bundle id '{eff}' diverges from the bundle's '{expected}', and \
                     this item requests manifest-backed runtime authority — its caps would be \
                     minted under '{eff}' and never match at dispatch. Namespace the item under \
                     '{expected}/…' or set --name."
                ),
            });
            continue;
        }
        // Inert cross-namespace item: verify against declared config shadows.
        if declared.contains(outcome.item_ref.as_str()) {
            matched.insert(outcome.item_ref.as_str());
            notes.push(sign_bundle::ItemWarning {
                item_ref: outcome.item_ref.clone(),
                message: format!(
                    "declared config shadow verified: ships '{}' under foreign namespace \
                     '{eff}' to override it via project-first resolution, as signed `shadows:` \
                     intent. No action needed.",
                    outcome.item_ref
                ),
            });
        } else if is_config_ref(&outcome.item_ref) {
            shadow_warnings.push(sign_bundle::ItemWarning {
                item_ref: outcome.item_ref.clone(),
                message: format!(
                    "undeclared override: ships config '{}' under foreign namespace '{eff}' with \
                     no matching `shadows:` declaration — declare it in manifest.source.yaml \
                     `shadows:` or rename it into '{expected}/…'.",
                    outcome.item_ref
                ),
            });
        } else {
            notes.push(sign_bundle::ItemWarning {
                item_ref: outcome.item_ref.clone(),
                message: format!(
                    "effective bundle id '{eff}' diverges from the bundle's '{expected}', but \
                     this item declares no manifest-backed runtime authority, so it cannot mint \
                     caps — an inert cross-namespace item. No action needed."
                ),
            });
        }
    }

    // Stale intent: declared shadows that no shipped item matched.
    for decl in shadows {
        if !matched.contains(decl.as_str()) {
            shadow_warnings.push(sign_bundle::ItemWarning {
                item_ref: decl.clone(),
                message: format!(
                    "stale `shadows:` declaration '{decl}' — no shipped item matches it; remove \
                     the declaration or ship the overriding item."
                ),
            });
        }
    }

    warnings.sort_by(|a, b| a.item_ref.cmp(&b.item_ref));
    shadow_warnings.sort_by(|a, b| a.item_ref.cmp(&b.item_ref));
    notes.sort_by(|a, b| a.item_ref.cmp(&b.item_ref));
    NamespaceLint {
        warnings,
        shadow_warnings,
        notes,
    }
}

/// Generate and sign `.ai/manifest.yaml` from `.ai/manifest.source.yaml`.
///
/// `name_override` is the **effective bundle id** the manifest must carry —
/// the first bare-id segment of the bundle's item refs, which runtime
/// authority requires to equal `manifest.name`. When `None`, the bundle
/// source directory's basename is used (the historical default, correct when
/// the directory name already matches the effective bundle id).
pub fn generate_and_sign_manifest(
    ai_dir: &Path,
    bundle_source: &Path,
    name_override: Option<&str>,
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

    let bundle_name = match name_override {
        Some(name) => name,
        None => bundle_source
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("bundle_source path has no directory name"))?,
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::sign_bundle::ItemOutcome;

    /// Build a validated-only report from `(item_ref, declares_runtime_authority)`
    /// pairs.
    fn report_from(items: &[(&str, bool)]) -> SignBundleReport {
        SignBundleReport {
            validated: items
                .iter()
                .map(|(r, ra)| ItemOutcome {
                    item_ref: (*r).to_string(),
                    error: None,
                    declares_runtime_authority: *ra,
                })
                .collect(),
            signed: Vec::new(),
            failed: Vec::new(),
            warnings: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn effective_id_is_first_bare_segment() {
        assert_eq!(item_effective_bundle_id("tool:arc/play"), Some("arc"));
        assert_eq!(
            item_effective_bundle_id("tool:ryeos/core/bundle/publish"),
            Some("ryeos")
        );
        assert_eq!(
            item_effective_bundle_id("service:bundle/sign"),
            Some("bundle")
        );
        assert_eq!(item_effective_bundle_id(""), None);
    }

    #[test]
    fn lint_warns_only_cap_minting_divergent_items() {
        // A divergent item that requests manifest-backed runtime authority is
        // an actionable warning; a matching-namespace cap item is clean.
        let report = report_from(&[
            ("tool:arc/play", true),
            ("tool:arc/solve", true),
            ("service:other/callback", true),
        ]);
        let lint = lint_item_namespaces(&report, "arc", &[]);
        assert_eq!(lint.warnings.len(), 1, "got {:?}", lint.warnings);
        assert_eq!(lint.warnings[0].item_ref, "service:other/callback");
        assert!(lint.warnings[0].message.contains("'other'"));
        assert!(lint.warnings[0].message.contains("'arc'"));
        assert!(lint.shadow_warnings.is_empty());
        assert!(lint.notes.is_empty());
    }

    #[test]
    fn lint_undeclared_config_shadow_warns() {
        // Foreign-namespace config items with no `shadows:` declaration are
        // undeclared overrides — advisory warnings, never cap-minting warnings.
        // A non-config inert item (knowledge) stays a plain note.
        let report = report_from(&[
            ("config:ryeos-runtime/execution", false),
            ("config:ryeos-runtime/limits", false),
            ("knowledge:other/notes", false),
        ]);
        let lint = lint_item_namespaces(&report, "downstream", &[]);
        assert!(
            lint.warnings.is_empty(),
            "undeclared shadows must not escalate: {:?}",
            lint.warnings
        );
        assert_eq!(
            lint.shadow_warnings.len(),
            2,
            "got {:?}",
            lint.shadow_warnings
        );
        assert!(lint.shadow_warnings[0]
            .message
            .contains("undeclared override"));
        assert_eq!(lint.notes.len(), 1, "got {:?}", lint.notes);
        assert_eq!(lint.notes[0].item_ref, "knowledge:other/notes");
    }

    #[test]
    fn lint_declared_config_shadow_verifies_as_note() {
        // A declared shadow that is actually shipped is verified: a note, not a
        // warning of any kind.
        let report = report_from(&[
            ("config:ryeos-runtime/execution", false),
            ("config:ryeos-runtime/limits", false),
        ]);
        let shadows = vec![
            "config:ryeos-runtime/execution".to_string(),
            "config:ryeos-runtime/limits".to_string(),
        ];
        let lint = lint_item_namespaces(&report, "downstream", &shadows);
        assert!(lint.warnings.is_empty());
        assert!(
            lint.shadow_warnings.is_empty(),
            "verified shadows must not warn: {:?}",
            lint.shadow_warnings
        );
        assert_eq!(lint.notes.len(), 2, "got {:?}", lint.notes);
        assert!(lint.notes[0]
            .message
            .contains("declared config shadow verified"));
    }

    #[test]
    fn lint_stale_shadow_declaration_warns() {
        // A `shadows:` declaration with no shipped item is stale intent.
        let report = report_from(&[("config:ryeos-runtime/execution", false)]);
        let shadows = vec![
            "config:ryeos-runtime/execution".to_string(),
            "config:ryeos-runtime/limits".to_string(),
        ];
        let lint = lint_item_namespaces(&report, "downstream", &shadows);
        assert!(lint.warnings.is_empty());
        assert_eq!(
            lint.shadow_warnings.len(),
            1,
            "got {:?}",
            lint.shadow_warnings
        );
        assert_eq!(
            lint.shadow_warnings[0].item_ref,
            "config:ryeos-runtime/limits"
        );
        assert!(lint.shadow_warnings[0].message.contains("stale"));
        // The shipped-and-declared shadow is verified as a note.
        assert_eq!(lint.notes.len(), 1);
    }

    #[test]
    fn lint_clean_when_all_items_match() {
        let report = report_from(&[("tool:arc/play", true), ("graph:arc/agent", false)]);
        let lint = lint_item_namespaces(&report, "arc", &[]);
        assert!(lint.warnings.is_empty());
        assert!(lint.shadow_warnings.is_empty());
        assert!(lint.notes.is_empty());
    }
}

#[cfg(test)]
mod runtime_authority_publish_tests {
    use super::manifest_declares_runtime_authority;

    #[test]
    fn detects_runtime_authority_declaration() {
        let tmp = tempfile::tempdir().unwrap();
        let ai = tmp.path().join(".ai");
        std::fs::create_dir_all(&ai).unwrap();
        // No manifest source → false.
        assert!(!manifest_declares_runtime_authority(&ai).unwrap());
        // Plain manifest → false.
        std::fs::write(
            ai.join("manifest.source.yaml"),
            "name: arc\nversion: \"0.1.0\"\n",
        )
        .unwrap();
        assert!(!manifest_declares_runtime_authority(&ai).unwrap());
        // runtime authority declared → true.
        std::fs::write(
            ai.join("manifest.source.yaml"),
            "name: arc\nversion: \"0.1.0\"\nruntime_authority:\n  bundle_events:\n    - event_kind: ev\n      operations: [append]\n",
        )
        .unwrap();
        assert!(manifest_declares_runtime_authority(&ai).unwrap());
    }
}
