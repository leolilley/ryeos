//! Bundle batch signer — maintainer-only, explicit roots, no ambient state.
//!
//! Signs every signable item in a bundle source tree using the provided
//! author key. Validates metadata anchoring for every item before signing.
//!
//! This is NOT the operator-side `ryos-core-tools` (which uses the user key and
//! resolves through the three-tier engine space). This is the bundle
//! authoring path: explicit `--source`, explicit `--registry-root`,
//! explicit `--key`. No ambient machine state.
//!
//! Usage:
//!   ryos publish sign-items \
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
use ryeos_engine::trust::{TrustedSigner, TrustStore};
use ryeos_engine::AI_DIR;

/// Report returned by [`sign_bundle_items`].
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignBundleReport {
    pub signed: Vec<ItemOutcome>,
    pub failed: Vec<ItemOutcome>,
}

impl SignBundleReport {
    pub fn is_total_success(&self) -> bool {
        self.failed.is_empty()
    }

    pub fn total(&self) -> usize {
        self.signed.len() + self.failed.len()
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
/// `registry_root` provides the kind schemas and parser tools for
/// validation. When signing core's own items, pass `--registry-root`
/// pointing at the same core bundle (after its kind schemas are
/// bootstrap-signed).
pub fn sign_bundle_items(
    source: &Path,
    registry_root: &Path,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<SignBundleReport> {
    let verifying_key = signing_key.verifying_key();
    let fingerprint = ryeos_engine::trust::compute_fingerprint(&verifying_key);

    // 1. Build a TrustStore seeded with ONLY the author's key
    let trust_store = TrustStore::from_signers(vec![TrustedSigner {
        fingerprint: fingerprint.clone(),
        verifying_key,
        label: Some("author".to_string()),
    }]);

    // 2. Load KindRegistry from registry_root ONLY (no system/user/project layering)
    let schema_root = registry_root.join(AI_DIR).join(ryeos_engine::KIND_SCHEMAS_DIR);
    if !schema_root.is_dir() {
        bail!(
            "registry root has no kind schemas at {}",
            schema_root.display()
        );
    }
    let kinds =
        KindRegistry::load_base(&[schema_root], &trust_store).context("load kind schemas")?;

    // 3. Build ParserDispatcher from registry_root.
    //    Requires: kind schemas AND parser tool YAMLs must already be signed
    //    (done by the bootstrap_sign_core_kind_schemas example, which signs
    //    both kind schemas and parser descriptors in one pass).
    let (parser_tools, _dups) =
        ParserRegistry::load_base(&[registry_root.to_path_buf()], &trust_store, &kinds)
            .context("load parser tools")?;
    let handlers = HandlerRegistry::load_base(&[(registry_root.to_path_buf(), ryeos_engine::resolution::TrustClass::TrustedSystem)], &trust_store)
        .context("load handler descriptors")?;
    let parser_dispatcher = ParserDispatcher::new(parser_tools, Arc::new(handlers));

    // 4. Walk every kind's directory under source/.ai/<kind_dir>/
    let ai_dir = source.join(AI_DIR);
    if !ai_dir.is_dir() {
        bail!("source has no .ai/ directory at {}", ai_dir.display());
    }

    let mut report = SignBundleReport {
        signed: Vec::new(),
        failed: Vec::new(),
    };

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

            let bare_id = derive_bare_id(&file_path, &kind_dir, kind_schema);
            let display_ref = format!("{}:{bare_id}", kind_name);

            match sign_one_item(
                &file_path,
                kind_schema,
                &ai_dir,
                &parser_dispatcher,
                signing_key,
            ) {
                Ok(()) => report.signed.push(ItemOutcome {
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

    Ok(report)
}

fn sign_one_item(
    file_path: &Path,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    ai_root: &Path,
    parsers: &ParserDispatcher,
    signing_key: &lillux::crypto::SigningKey,
) -> Result<()> {
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("read {}", file_path.display()))?;

    let ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .ok_or_else(|| anyhow::anyhow!("file has no extension: {}", file_path.display()))?;

    let source_format = kind_schema
        .resolved_format_for(&ext)
        .ok_or_else(|| {
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

    // Sign in place (atomic)
    let stripped = lillux::signature::strip_signature_lines(&content);
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

    Ok(())
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
