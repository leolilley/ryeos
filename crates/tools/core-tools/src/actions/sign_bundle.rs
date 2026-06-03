//! Bundle batch signer — maintainer-only, explicit roots, no ambient state.
//!
//! Signs every signable item in a bundle source tree using the provided
//! author key. Validates metadata anchoring for every item before signing.
//!
//! This is NOT the operator-side `ryeos-core-tools` (which uses the user key and
//! resolves through the three-tier engine space). This is the bundle
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
        .map(|root| (root, ryeos_engine::resolution::TrustClass::TrustedSystem))
        .collect();
    if !registry_roots.iter().any(|root| root == source) {
        handler_roots.push((
            source.to_path_buf(),
            ryeos_engine::resolution::TrustClass::TrustedSystem,
        ));
    }
    let handlers = HandlerRegistry::load_base(&handler_roots, &trust_store)
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
            let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

            if kind_schema.spec_for(&format!(".{ext}")).is_none() {
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
                Ok(SignResult::Unchanged) => report.validated.push(ItemOutcome {
                    item_ref: display_ref,
                    error: None,
                }),
                Ok(SignResult::Signed) => report.signed.push(ItemOutcome {
                    item_ref: display_ref,
                    error: None,
                }),
                Err(e) => report.failed.push(ItemOutcome {
                    item_ref: display_ref,
                    error: Some(format!("{e:#}")),
                }),
            }
        }
    }

    report.validated.sort_by(|a, b| a.item_ref.cmp(&b.item_ref));
    report.signed.sort_by(|a, b| a.item_ref.cmp(&b.item_ref));
    report.failed.sort_by(|a, b| a.item_ref.cmp(&b.item_ref));

    Ok(report)
}

/// Outcome of signing a single item.
enum SignResult {
    /// Already valid, left untouched.
    Unchanged,
    /// (Re-)signed in place.
    Signed,
}

fn sign_one_item(
    file_path: &Path,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    ai_root: &Path,
    parsers: &ParserDispatcher,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<SignResult> {
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
        return Ok(SignResult::Unchanged);
    }

    // Sign in place (atomic)
    let signed = lillux::signature::sign_content(
        &stripped,
        signing_key,
        &source_format.signature.prefix,
        source_format.signature.suffix.as_deref(),
    );

    let tmp = file_path.with_extension(format!("signed.tmp.{}", std::process::id()));
    fs::write(&tmp, &signed).with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, file_path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), file_path.display()))?;

    Ok(SignResult::Signed)
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
    lillux::signature::is_valid_signature_for(
        &header.content_hash,
        &header.signature_b64,
        &header.signer_fingerprint,
        body,
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
