//! Bundle batch signer — maintainer-only, explicit roots, no ambient state.
//!
//! Signs every signable item in a bundle source tree using the provided
//! author key. Validates metadata anchoring for every item before signing.
//!
//! This is NOT the operator-side `ryeos-core-tools` (which uses the user key and
//! resolves through the engine space). This is the bundle
//! authoring path: explicit `--source`, explicit `--registry-root`,
//! explicit `--key`. No ambient machine state.
//!
//! Usage:
//!   ryeos publish sign-items \
//!       --source <bundle-root>          \
//!       --registry-root <signed-core>   \
//!       --key <author.pem>

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use ryeos_engine::kind_registry::{validate_metadata_anchoring, KindRegistry};
use std::sync::Arc;

use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::{TrustStore, TrustedSigner};
use ryeos_engine::AI_DIR;

/// Report returned by [`sign_bundle_items`].
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignBundleReport {
    /// Items already valid and left untouched.
    pub validated: Vec<ItemOutcome>,
    /// Items (re-)signed because unsigned, invalid, wrong signer, or content changed.
    pub signed: Vec<ItemOutcome>,
    pub failed: Vec<ItemOutcome>,
    /// Authoring warnings that require action (e.g. a cap-minting item whose
    /// effective bundle id diverges from the bundle's, which the daemon would
    /// reject at runtime). Empty on a clean tree. Populated by the publish
    /// path, not the signer loop itself.
    #[serde(default)]
    pub warnings: Vec<ItemWarning>,
    /// Informational notes that need no action (e.g. an inert cross-namespace
    /// item such as a project-first config shadow). Surfaced, never
    /// error-escalated. Populated by the publish path.
    #[serde(default)]
    pub notes: Vec<ItemWarning>,
}

impl SignBundleReport {
    pub fn is_total_success(&self) -> bool {
        self.failed.is_empty()
    }

    pub fn total(&self) -> usize {
        self.validated.len() + self.signed.len() + self.failed.len()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemOutcome {
    pub item_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// True when the item declares manifest-backed runtime-authority
    /// requirements (`requires.capabilities.manifest.runtime_authority`) and so
    /// participates in daemon cap minting. Drives namespace-lint precision:
    /// only such items make a namespace divergence actionable, because only
    /// they can mint caps under the wrong effective bundle id.
    #[serde(default)]
    pub declares_runtime_authority: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemWarning {
    pub item_ref: String,
    pub message: String,
}

/// Sign every signable item in the bundle at `source` using `signing_key`.
///
/// `registry_roots` provide the kind schemas and parser tools for validation.
/// When signing core's own items, pass the core bundle itself (after its kind
/// schemas are bootstrap-signed). For bundles with transitive dependencies,
/// pass every dependency root in dependency order.
pub fn sign_bundle_items(
    source: &Path,
    registry_roots: &[PathBuf],
    signing_key: &lillux::crypto::SigningKey,
) -> Result<SignBundleReport> {
    sign_bundle_items_with_trust(source, registry_roots, signing_key, None)
}

/// Sign every signable item in the bundle at `source` using `signing_key`,
/// while also trusting registry/dependency signer keys from `base_trust_store`.
///
/// This is intended for descriptor-driven offline authoring services, where
/// kind schemas and parser descriptors come from already-installed bundles
/// signed by the platform publisher, while source items are signed by the
/// current user key.
pub fn sign_bundle_items_with_trust(
    source: &Path,
    registry_roots: &[PathBuf],
    signing_key: &lillux::crypto::SigningKey,
    base_trust_store: Option<&TrustStore>,
) -> Result<SignBundleReport> {
    sign_bundle_generation(source, registry_roots, signing_key, base_trust_store, false)
}

/// Rebuild binary CAS metadata and sign every item as one atomic bundle
/// generation. This is the public `bundle sign` workflow: neither the rebuilt
/// refs nor a subset of new signatures becomes visible if any signing step
/// fails.
pub fn rebuild_and_sign_bundle_items_with_trust(
    source: &Path,
    registry_roots: &[PathBuf],
    signing_key: &lillux::crypto::SigningKey,
    base_trust_store: Option<&TrustStore>,
) -> Result<SignBundleReport> {
    sign_bundle_generation(source, registry_roots, signing_key, base_trust_store, true)
}

fn sign_bundle_generation(
    source: &Path,
    registry_roots: &[PathBuf],
    signing_key: &lillux::crypto::SigningKey,
    base_trust_store: Option<&TrustStore>,
    rebuild_binary_manifest: bool,
) -> Result<SignBundleReport> {
    let live_root = source.to_path_buf();
    super::publisher_transaction::with_staged_bundle_generation(source, |staging| {
        let staged_registry_roots = super::publisher_transaction::roots_for_staged_generation(
            &live_root,
            staging,
            registry_roots,
        );
        if rebuild_binary_manifest && staging.join(AI_DIR).join("bin").is_dir() {
            super::build_bundle::rebuild_bundle_manifest_in_place(staging, signing_key)
                .context("rebuild source bundle binary manifest")?;
        }

        let mut report = sign_bundle_items_with_trust_in_place(
            staging,
            &staged_registry_roots,
            signing_key,
            base_trust_store,
        )?;
        remap_report_errors(&mut report, staging, &live_root);
        if !report.is_total_success() {
            let failures = report
                .failed
                .iter()
                .map(|outcome| match &outcome.error {
                    Some(error) => format!("{}: {error}", outcome.item_ref),
                    None => outcome.item_ref.clone(),
                })
                .collect::<Vec<_>>()
                .join("; ");
            bail!(
                "bundle sign failed for {} of {} item(s): {}",
                report.failed.len(),
                report.total(),
                failures
            );
        }
        Ok(report)
    })
}

fn remap_report_errors(report: &mut SignBundleReport, staging: &Path, live_root: &Path) {
    let staged = staging.to_string_lossy();
    let live = live_root.to_string_lossy();
    for outcome in report
        .validated
        .iter_mut()
        .chain(report.signed.iter_mut())
        .chain(report.failed.iter_mut())
    {
        if let Some(error) = &mut outcome.error {
            *error = error.replace(staged.as_ref(), live.as_ref());
        }
    }
}

/// Sign items directly in a caller-owned private generation.
pub(super) fn sign_bundle_items_with_trust_in_place(
    source: &Path,
    registry_roots: &[PathBuf],
    signing_key: &lillux::crypto::SigningKey,
    base_trust_store: Option<&TrustStore>,
) -> Result<SignBundleReport> {
    let verifying_key = signing_key.verifying_key();
    let fingerprint = ryeos_engine::trust::compute_fingerprint(&verifying_key);

    // 1. Build a TrustStore seeded with the author's key plus any
    //    caller-provided registry/dependency trust.
    let mut trust_store = TrustStore::from_signers(vec![TrustedSigner {
        fingerprint: fingerprint.clone(),
        verifying_key,
        label: Some("author".to_string()),
    }]);
    if let Some(base) = base_trust_store {
        trust_store.extend_from(base);
    }

    // 2. Load KindRegistry from registry_roots AND source bundle's own kind
    //    schemas. Registry roots provide base/dependency kinds (core, standard,
    //    etc.); the source bundle may provide additional kinds.
    let mut schema_roots = Vec::new();
    for registry_root in registry_roots {
        let registry_schema_root = registry_root
            .join(AI_DIR)
            .join(ryeos_engine::KIND_SCHEMAS_DIR);
        if registry_schema_root.is_dir() {
            schema_roots.push(registry_schema_root);
        }
    }
    let source_schema_root = source.join(AI_DIR).join(ryeos_engine::KIND_SCHEMAS_DIR);
    if source_schema_root.is_dir() {
        schema_roots.push(source_schema_root);
    }
    if schema_roots.is_empty() {
        bail!("no kind schemas found in registry root or source bundle");
    }
    let kinds =
        KindRegistry::load_base(&schema_roots, &trust_store).context("load kind schemas")?;

    // 3. Build ParserDispatcher from registry_roots AND source bundle.
    //    Requires: kind schemas AND parser tool YAMLs must already be signed
    //    (done by the bootstrap_sign_core_kind_schemas example, which signs
    //    both kind schemas and parser descriptors in one pass).
    let mut parser_roots = registry_roots.to_vec();
    if !parser_roots.iter().any(|root| root == source) {
        parser_roots.push(source.to_path_buf());
    }
    let (parser_tools, _dups) = ParserRegistry::load_base(&parser_roots, &trust_store, &kinds)
        .context("load parser tools")?;
    let mut handler_roots: Vec<(PathBuf, ryeos_engine::resolution::TrustClass)> = registry_roots
        .iter()
        .cloned()
        .map(|root| (root, ryeos_engine::resolution::TrustClass::TrustedBundle))
        .collect();
    if !registry_roots.iter().any(|root| root == source) {
        handler_roots.push((
            source.to_path_buf(),
            ryeos_engine::resolution::TrustClass::TrustedBundle,
        ));
    }
    let handlers = HandlerRegistry::load_base_for_authoring(&handler_roots, &trust_store)
        .context("load handler descriptors")?;
    let parser_dispatcher = ParserDispatcher::new(parser_tools, Arc::new(handlers));

    // 4. Walk every kind's directory under source/.ai/<kind_dir>/
    let ai_dir = source.join(AI_DIR);
    if !ai_dir.is_dir() {
        bail!("source has no .ai/ directory at {}", ai_dir.display());
    }

    let mut report = SignBundleReport {
        validated: Vec::new(),
        signed: Vec::new(),
        failed: Vec::new(),
        warnings: Vec::new(),
        notes: Vec::new(),
    };

    let mut kind_names: Vec<String> = kinds.kinds().map(str::to_owned).collect();
    kind_names.sort();

    for kind_name in kind_names {
        let kind_schema = match kinds.get(&kind_name) {
            Some(s) => s,
            None => continue,
        };
        let kind_dir = ai_dir.join(&kind_schema.directory);
        if !kind_dir.is_dir() {
            continue;
        }

        let mut files: Vec<PathBuf> = Vec::new();
        collect_files_recursive(&kind_dir, &mut files);
        files.sort();

        for file_path in files {
            // Node runtime state and signing secrets are never signable source.
            // A kind walk over `.ai/node/{schedules,routes,bundles}` or
            // `.ai/state`/`.ai/knowledge` may sweep files a running daemon
            // wrote; excluding them here keeps them out of the report entirely,
            // so they can neither fail as items nor raise namespace warnings.
            if crate::actions::runtime_owned::is_runtime_owned_file(&file_path, &ai_dir) {
                continue;
            }

            let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

            if kind_schema.spec_for(&format!(".{ext}")).is_none() {
                continue;
            }
            if is_bundle_tool_runtime_lib(&file_path, &kind_dir, kind_schema) {
                continue;
            }

            let bare_id = derive_bare_id(&file_path, &kind_dir, kind_schema);
            let display_ref = format!("{}:{bare_id}", kind_name);

            match sign_one_item(
                &file_path,
                kind_schema,
                &ai_dir,
                &parser_dispatcher,
                signing_key,
            ) {
                Ok(info) => {
                    let outcome = ItemOutcome {
                        item_ref: display_ref,
                        error: None,
                        declares_runtime_authority: info.declares_runtime_authority,
                    };
                    match info.result {
                        SignResult::Unchanged => report.validated.push(outcome),
                        SignResult::Signed => report.signed.push(outcome),
                    }
                }
                Err(e) => report.failed.push(ItemOutcome {
                    item_ref: display_ref,
                    error: Some(format!("{e:#}")),
                    declares_runtime_authority: false,
                }),
            }
        }
    }

    // Loud pipeline: a populated item directory with no registered kind would
    // otherwise be skipped above with only a TRACE line. Fail before returning
    // an incomplete report.
    check_all_item_dirs_covered(&ai_dir, &kinds)?;

    report.validated.sort_by(|a, b| a.item_ref.cmp(&b.item_ref));
    report.signed.sort_by(|a, b| a.item_ref.cmp(&b.item_ref));
    report.failed.sort_by(|a, b| a.item_ref.cmp(&b.item_ref));

    Ok(report)
}

/// Fail loudly when a populated `.ai/<dir>` is not covered by any registered
/// kind. The kind loop only visits directories for kinds present in the loaded
/// registry; an item directory whose kind lives in a bundle that was neither
/// passed as a registry root nor defined by the source would otherwise be
/// skipped with only a TRACE line, silently dropping those items from the
/// signed bundle. This makes the "publish with the right registry roots"
/// ordering enforced instead of advisory.
fn check_all_item_dirs_covered(ai_dir: &Path, kinds: &KindRegistry) -> Result<()> {
    // Top-level `.ai/` directory each registered kind owns.
    let mut covered: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for kind_name in kinds.kinds() {
        if let Some(schema) = kinds.get(kind_name) {
            if let Some(top) = schema.directory.split('/').next() {
                if !top.is_empty() {
                    covered.insert(top.to_string());
                }
            }
        }
    }

    let entries = match fs::read_dir(ai_dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Derived CAS + binary artifacts are never item directories.
        if matches!(name, "objects" | "refs" | "bin") {
            continue;
        }
        // A directory some registered kind already walks.
        if covered.contains(name) {
            continue;
        }
        // Node runtime state / signing secrets are not signable source.
        if crate::actions::runtime_owned::is_runtime_owned_ai_path(&format!("{AI_DIR}/{name}")) {
            continue;
        }
        // Empty directories sign nothing — only populated ones are a problem.
        if !dir_contains_file(&path) {
            continue;
        }
        bail!(
            "bundle has items under `{name}/`, but no registered kind covers that directory \
             (the `{name}` kind is absent from the loaded registry roots and the bundle's own \
             kind schemas). Those items would be silently skipped. Pass the registry root of the \
             bundle that defines the `{name}` kind (for example --registry-root <standard> for \
             the `knowledge` kind), or add its kind schema to this bundle."
        );
    }
    Ok(())
}

/// True when `dir` contains at least one regular file anywhere below it.
fn dir_contains_file(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() {
            return true;
        }
        if p.is_dir() && dir_contains_file(&p) {
            return true;
        }
    }
    false
}

/// Outcome of signing a single item.
enum SignResult {
    /// Already valid, left untouched.
    Unchanged,
    /// (Re-)signed in place.
    Signed,
}

/// Signing result plus the runtime-authority fact the namespace lint needs.
struct SignItemInfo {
    result: SignResult,
    /// Whether the parsed item declares manifest-backed runtime-authority
    /// requirements (`requires.capabilities.manifest.runtime_authority`).
    declares_runtime_authority: bool,
}

fn sign_one_item(
    file_path: &Path,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    ai_root: &Path,
    parsers: &ParserDispatcher,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<SignItemInfo> {
    let content =
        fs::read_to_string(file_path).with_context(|| format!("read {}", file_path.display()))?;

    let ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .ok_or_else(|| anyhow::anyhow!("file has no extension: {}", file_path.display()))?;

    let source_format = kind_schema.resolved_format_for(&ext).ok_or_else(|| {
        anyhow::anyhow!(
            "extension `{ext}` not registered for kind `{}`",
            kind_schema.directory
        )
    })?;

    // Parse
    let parsed = parsers
        .dispatch(
            &source_format.parser,
            &content,
            Some(file_path),
            &source_format.signature,
        )
        .with_context(|| format!("parse {}", file_path.display()))?;

    // Validate metadata anchoring — ALWAYS on, no skip
    validate_metadata_anchoring(
        &parsed,
        &kind_schema.extraction_rules,
        &kind_schema.directory,
        ai_root,
        file_path,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "path-anchoring validator refused {}: {e}",
            file_path.display()
        )
    })?;

    // Does this item participate in daemon cap minting? Only items whose
    // `requires.capabilities.manifest.runtime_authority` is non-empty do; the
    // namespace lint uses this to distinguish a cap-minting item (actionable
    // namespace divergence) from an inert cross-namespace item (a note).
    let declares_runtime_authority = declares_manifest_runtime_authority(&parsed);

    // Strip existing signature to get the canonical body
    let envelope = ryeos_engine::contracts::SignatureEnvelope {
        prefix: source_format.signature.prefix.clone(),
        suffix: source_format.signature.suffix.clone(),
        after_shebang: source_format.signature.after_shebang,
    };
    let stripped = lillux::signature::strip_signature_lines_with_envelope(
        &content,
        &envelope.prefix,
        envelope.suffix.as_deref(),
    );

    // Check if the existing signature is already valid for this body and signer.
    if is_already_validly_signed(&content, &stripped, signing_key, &envelope) {
        return Ok(SignItemInfo {
            result: SignResult::Unchanged,
            declares_runtime_authority,
        });
    }

    // Sign in place (atomic)
    let signed = lillux::signature::sign_content_with_options(
        &stripped,
        signing_key,
        &source_format.signature.prefix,
        source_format.signature.suffix.as_deref(),
        source_format.signature.after_shebang,
    );

    let tmp = file_path.with_extension(format!("signed.tmp.{}", std::process::id()));
    fs::write(&tmp, &signed).with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, file_path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), file_path.display()))?;

    Ok(SignItemInfo {
        result: SignResult::Signed,
        declares_runtime_authority,
    })
}

/// True when a parsed item declares manifest-backed runtime-authority
/// requirements — a non-empty `requires.capabilities.manifest.runtime_authority`
/// block. Mirrors the daemon mint predicate: the same
/// [`ryeos_bundle::runtime_authority::parse_runtime_requires`] the launch path
/// uses, so lint precision tracks the actual cap-minting boundary. A missing or
/// malformed `requires` block declares no runtime authority.
fn declares_manifest_runtime_authority(parsed: &serde_json::Value) -> bool {
    parsed
        .get("requires")
        .and_then(|rv| ryeos_bundle::runtime_authority::parse_runtime_requires(rv).ok())
        .map(|reqs| reqs.manifest.runtime_authority.declares_runtime_authority())
        .unwrap_or(false)
}

/// Check whether `existing` (the full file content) already carries a
/// valid signature for `body` (the stripped content) signed by `signing_key`.
///
/// Returns true only when all three conditions hold:
///   1. the parsed header's content hash matches the body,
///   2. the signer fingerprint matches the current key, and
///   3. the signature verifies against the hash.
fn is_already_validly_signed(
    existing: &str,
    body: &str,
    signing_key: &lillux::crypto::SigningKey,
    envelope: &ryeos_engine::contracts::SignatureEnvelope,
) -> bool {
    let Some(header) = ryeos_engine::item_resolution::parse_signature_header(existing, envelope)
    else {
        return false;
    };

    let verifying_key = signing_key.verifying_key();
    let fingerprint = lillux::signature::compute_fingerprint(&verifying_key);
    let signed_body = lillux::signature::content_to_sign(body, envelope.after_shebang);
    lillux::signature::is_valid_signature_for(
        &header.content_hash,
        &header.signature_b64,
        &header.signer_fingerprint,
        signed_body,
        &verifying_key,
        &fingerprint,
    )
}

/// Derive a bare-id from a file path relative to its kind directory,
/// stripping the extension.
fn derive_bare_id(
    file_path: &Path,
    kind_dir: &Path,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
) -> String {
    if let Ok(rel) = file_path.strip_prefix(kind_dir) {
        let s = rel.to_string_lossy().to_string();
        for spec in &kind_schema.extensions {
            if let Some(stripped) = s.strip_suffix(&spec.ext) {
                return stripped.to_string();
            }
        }
        s
    } else {
        file_path.display().to_string()
    }
}

/// Runtime support modules live below `.ai/tools/**/lib/` so runtime descriptors
/// can pass `{runtime_dir}/lib` to controlled launchers. They are source
/// dependencies of the runtime item, not standalone `tool:`/`streaming_tool:`
/// item roots, so the bundle item signer must not path-anchor them as tools.
fn is_bundle_tool_runtime_lib(
    file_path: &Path,
    kind_dir: &Path,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
) -> bool {
    if kind_schema.directory != "tools" {
        return false;
    }
    is_tools_python_lib_path(file_path, kind_dir)
}

fn is_tools_python_lib_path(file_path: &Path, kind_dir: &Path) -> bool {
    if file_path.extension().and_then(|e| e.to_str()) != Some("py") {
        return false;
    }
    let Ok(rel) = file_path.strip_prefix(kind_dir) else {
        return false;
    };
    rel.components()
        .any(|component| component.as_os_str() == "lib")
}

/// Recursively collect all files under a directory.
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

    #[test]
    fn skips_python_runtime_lib_dependencies_under_tools() {
        let kind_dir = Path::new("/bundle/.ai/tools");

        assert!(is_tools_python_lib_path(
            Path::new("/bundle/.ai/tools/ryeos/core/runtimes/python/lib/support.py"),
            kind_dir,
        ));
        assert!(!is_tools_python_lib_path(
            Path::new("/bundle/.ai/tools/ryeos/core/verbs/list.py"),
            kind_dir,
        ));
        assert!(!is_tools_python_lib_path(
            Path::new("/bundle/.ai/tools/ryeos/core/runtimes/python/function.yaml"),
            kind_dir,
        ));
    }
}
