//! Unified bundle binary resolution for runtimes + handlers.
//!
//! Accepts two intentional cross-consumer ref shapes — neither is a
//! compatibility shim; they're the canonical forms used by different
//! authoring surfaces and the resolver normalizes both to the same
//! verified `<bundle>/.ai/bin/<host_triple>/<name>` path:
//!
//!   - `bin:<name>` — the canonical short form used by tool YAMLs.
//!     The triple is implicit (always the host triple), so authors
//!     don't have to mention it; it's also the form `ryeos publish`
//!     emits when describing tool binary refs.
//!
//!   - `bin/<triple>/<name>` — the path-style form used by runtime
//!     YAMLs and handler descriptors. The triple is explicit so a
//!     bundle can ship multiple architectures side-by-side and the
//!     descriptor unambiguously names which one it covers.
//!
//!   - `bin/{triple}/<name>` — the literal-placeholder variant of the
//!     path-style form. Authors who want the explicit `bin/<triple>/<name>`
//!     shape (so a descriptor visibly declares it's pointing at an
//!     architecture-namespaced binary) but don't want to hard-code one
//!     architecture write `{triple}` and the resolver substitutes the
//!     host triple before the normal explicit-path verification runs.
//!
//! All three shapes go through the same manifest-hash + trust-store
//! verification path below.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lillux::crypto::VerifyingKey;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::contracts::{ItemSpace, SignatureEnvelope, TrustClass as ContractTrustClass};
use crate::error::EngineError;
use crate::item_resolution::ResolutionRoots;
use crate::resolution::TrustClass;
use crate::trust::TrustStore;

/// Result of resolving a binary reference.
#[derive(Debug)]
pub struct ResolvedBinary {
    pub absolute_path: PathBuf,
    pub manifest_hash: String,
    pub signer_fingerprint: String,
}

/// Resolve a runtime command binary ref for a concrete wrapper item.
///
/// Accepted shapes:
///   - `bin:<name>` — existing behavior, resolved in the wrapper item's bundle.
///   - `bin:<bundle>/<name>` — qualified behavior, resolved in the registered
///     signed bundle whose manifest name is `<bundle>`.
///
/// Qualified refs intentionally change only executable materialization. Runtime
/// callback authority remains attached to the wrapper item that requested the
/// subprocess, not to the bundle that ships the binary.
pub fn resolve_runtime_binary_command_ref(
    binary_ref: &str,
    wrapper_source_path: &Path,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
    root_trust_class: TrustClass,
) -> Result<ResolvedBinary, EngineError> {
    let Some(rest) = binary_ref.strip_prefix("bin:") else {
        return Err(EngineError::InvalidBinPrefix {
            raw: binary_ref.to_string(),
            detail: "runtime command binary_ref must start with `bin:`".into(),
        });
    };

    let slash_count = rest.matches('/').count();
    match slash_count {
        0 => {
            let bundle_root = registered_bundle_root_for_source(wrapper_source_path, roots)
                .or_else(|| find_bundle_root(wrapper_source_path))
                .ok_or_else(|| EngineError::InvalidBinPrefix {
                    raw: binary_ref.to_string(),
                    detail: format!(
                        "cannot find bundle root (no registered bundle or .ai/ ancestor of {})",
                        wrapper_source_path.display()
                    ),
                })?;
            resolve_bundle_binary_ref(
                binary_ref,
                &bundle_root,
                |fp| trust_store.get(fp).map(|signer| signer.verifying_key),
                root_trust_class,
            )
        }
        1 => {
            let (target_bundle, bin_name) = rest.split_once('/').expect("one slash checked above");
            validate_bundle_name(target_bundle, binary_ref)?;
            validate_bin_name(bin_name, binary_ref)?;

            let source_root = registered_bundle_root_for_source(wrapper_source_path, roots)
                .ok_or_else(|| EngineError::InvalidBinPrefix {
                    raw: binary_ref.to_string(),
                    detail: format!(
                        "qualified `bin:<bundle>/<name>` refs are only allowed from registered bundle items; {} is not under a registered bundle root",
                        wrapper_source_path.display()
                    ),
                })?;
            let source_manifest = load_minimal_bundle_manifest(&source_root, trust_store)?;
            let (target_root, target_manifest) =
                find_qualified_target_bundle(target_bundle, roots, trust_store)?;

            ensure_qualified_binary_dependency(&source_manifest, &target_manifest)?;

            resolve_bundle_binary_ref(
                &format!("bin:{bin_name}"),
                &target_root,
                |fp| trust_store.get(fp).map(|signer| signer.verifying_key),
                root_trust_class,
            )
        }
        _ => Err(EngineError::InvalidBinPrefix {
            raw: binary_ref.to_string(),
            detail: "qualified binary refs must be `bin:<bundle>/<binary>`; bundle and binary names cannot contain `/`".into(),
        }),
    }
}

/// Resolve a binary reference relative to a bundle root.
///
/// Accepted shapes:
///   - `bin:<name>` — name resolved against `<bundle>/.ai/bin/<host_triple>/`
///   - `bin/<triple>/<name>` — explicit triple path (must equal host triple)
///
/// Verification:
///   - The file must exist at the resolved path.
///   - The bundle's manifest must contain an entry for the binary
///     whose hash matches the on-disk content.
///   - The manifest's signer must be present in the trust store.
///
/// Errors are EngineError variants — InvalidBinPrefix, BinNotFound,
/// BinHashMismatch, BinUntrusted (existing variants from foundation
/// wave).
pub fn resolve_bundle_binary_ref(
    binary_ref: &str,
    bundle_root: &Path,
    trusted_verifying_key: impl Fn(&str) -> Option<VerifyingKey>,
    root_trust_class: TrustClass,
) -> Result<ResolvedBinary, EngineError> {
    let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");

    // Determine the binary name and item_ref based on ref shape.
    let (bin_name, item_ref, bin_path) = if let Some(name) = binary_ref.strip_prefix("bin:") {
        // Canonical short shape: bin:<name> (triple implicit = host)
        let name = name.trim();
        validate_bin_name(name, binary_ref)?;
        let path = bundle_root
            .join(crate::AI_DIR)
            .join("bin")
            .join(triple)
            .join(name);
        let iref = format!("bin/{triple}/{name}");
        (name.to_string(), iref, path)
    } else if binary_ref.starts_with("bin/") {
        // Path-style shape: bin/<triple>/<name>
        //
        // Authors may write the literal placeholder `{triple}` in the
        // triple segment; we substitute the host triple here so the
        // descriptor stays portable across architectures while still
        // visibly carrying the architecture-namespaced shape.
        let parts: Vec<&str> = binary_ref.splitn(4, '/').collect();
        if parts.len() != 3 {
            return Err(EngineError::InvalidBinPrefix {
                raw: binary_ref.to_string(),
                detail: "path-style binary_ref must be `bin/<triple>/<name>`".into(),
            });
        }
        let raw_ref_triple = parts[1];
        let name = parts[2];

        let ref_triple = if raw_ref_triple == "{triple}" {
            triple
        } else {
            raw_ref_triple
        };

        if ref_triple != triple {
            return Err(EngineError::InvalidBinPrefix {
                raw: binary_ref.to_string(),
                detail: format!(
                    "binary_ref triple `{ref_triple}` doesn't match host triple `{triple}`"
                ),
            });
        }

        validate_bin_name(name, binary_ref)?;

        let path = bundle_root
            .join(crate::AI_DIR)
            .join("bin")
            .join(triple)
            .join(name);
        // Normalize the item_ref used for manifest lookup to the
        // resolved triple so manifests don't have to track placeholders.
        let iref = format!("bin/{triple}/{name}");
        (name.to_string(), iref, path)
    } else {
        return Err(EngineError::InvalidBinPrefix {
            raw: binary_ref.to_string(),
            detail: "binary_ref must start with `bin:` or `bin/<triple>/`".into(),
        });
    };

    // --- Common verification path ---

    let bin_dir = bin_path.parent().ok_or_else(|| EngineError::BinNotFound {
        bin: bin_name.clone(),
        searched: "cannot determine binary directory".into(),
    })?;

    if !bin_dir.is_dir() {
        return Err(EngineError::BinNotFound {
            bin: bin_name.clone(),
            searched: format!("expected triple dir {}", bin_dir.display()),
        });
    }

    if !bin_path.exists() {
        return Err(EngineError::BinNotFound {
            bin: bin_name.clone(),
            searched: bin_path.display().to_string(),
        });
    }

    // --- Confinement: resolved path must be under the canonical bin dir ---

    let canonical_bin_dir = bin_dir.canonicalize().map_err(|e| {
        EngineError::Internal(format!(
            "failed to canonicalize bin dir {}: {e}",
            bin_dir.display()
        ))
    })?;
    let canonical_resolved = bin_path.canonicalize().map_err(|e| {
        EngineError::Internal(format!(
            "failed to canonicalize resolved path {}: {e}",
            bin_path.display()
        ))
    })?;

    // Verify the canonical resolved path starts with the canonical bin dir.
    if !canonical_resolved.starts_with(&canonical_bin_dir) {
        return Err(EngineError::BinOutsideBundle {
            bin: bin_name.clone(),
            resolved: canonical_resolved.display().to_string(),
            bin_dir: canonical_bin_dir.display().to_string(),
        });
    }

    // --- Regular file check: reject symlinks that escape, FIFOs, dirs, devices ---

    let metadata = std::fs::metadata(&canonical_resolved).map_err(|e| {
        EngineError::Internal(format!(
            "failed to stat resolved binary {}: {e}",
            canonical_resolved.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(EngineError::BinNotRegularFile {
            bin: bin_name.clone(),
        });
    }

    let manifest_ref_path = bundle_root
        .join(crate::AI_DIR)
        .join("refs")
        .join("bundles")
        .join("manifest");

    if !manifest_ref_path.exists() {
        return Err(EngineError::BinManifestMissing {
            bundle_root: bundle_root.display().to_string(),
        });
    }

    let manifest_hash = std::fs::read_to_string(&manifest_ref_path)
        .map_err(|_| EngineError::BinManifestMissing {
            bundle_root: bundle_root.display().to_string(),
        })?
        .trim()
        .to_string();

    let objects_dir = bundle_root.join(crate::AI_DIR).join("objects");
    let cas = lillux::cas::CasStore::new(objects_dir);

    let manifest_value = cas
        .get_object(&manifest_hash)
        .map_err(|e| {
            EngineError::Internal(format!("CAS read error for manifest {manifest_hash}: {e}"))
        })?
        .ok_or_else(|| EngineError::BinManifestMissing {
            bundle_root: bundle_root.display().to_string(),
        })?;

    let item_source_hashes: HashMap<String, String> = manifest_value
        .get("item_source_hashes")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();

    let item_source_hash =
        item_source_hashes
            .get(&item_ref)
            .ok_or_else(|| EngineError::BinNotInManifest {
                bin: bin_name.clone(),
                triple: triple.to_string(),
            })?;

    let item_source = cas
        .get_object(item_source_hash)
        .map_err(|e| {
            EngineError::Internal(format!(
                "CAS read error for item_source {item_source_hash}: {e}"
            ))
        })?
        .ok_or_else(|| {
            EngineError::Internal(format!(
                "item_source {item_source_hash} for {item_ref} not found in CAS"
            ))
        })?;

    let signed_sidecar_fingerprint = verify_item_source_sidecar(
        &bin_name,
        &bin_path,
        &item_ref,
        &item_source,
        &trusted_verifying_key,
    )?;

    let content_blob_hash = item_source
        .get("content_blob_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let bin_bytes = std::fs::read(&bin_path).map_err(|e| {
        EngineError::Internal(format!("failed to read binary {}: {e}", bin_path.display()))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bin_bytes);
    let computed_hash = format!("{:x}", hasher.finalize());

    if computed_hash != content_blob_hash {
        return Err(EngineError::BinHashMismatch {
            bin: bin_name.clone(),
            declared: content_blob_hash,
            computed: computed_hash,
        });
    }

    let (trust_class, fingerprint) = crate::executor_resolution::verify_executor_trust(
        &item_source,
        |fp| trusted_verifying_key(fp).is_some(),
        root_trust_class,
    );

    if !is_dispatchable_trust_class(trust_class) {
        return Err(EngineError::BinUntrusted {
            bin: bin_name,
            fingerprint: fingerprint.unwrap_or_else(|| "<unknown>".to_string()),
        });
    }
    let signer_fingerprint = fingerprint.unwrap_or_else(|| "<unknown>".to_string());
    if signer_fingerprint != signed_sidecar_fingerprint {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name,
            reason: format!(
                "sidecar signer `{signed_sidecar_fingerprint}` does not match item_source signature_info fingerprint `{signer_fingerprint}`"
            ),
        });
    }

    Ok(ResolvedBinary {
        absolute_path: bin_path,
        manifest_hash,
        signer_fingerprint,
    })
}

fn registered_bundle_root_for_source(
    source_path: &Path,
    roots: &ResolutionRoots,
) -> Option<PathBuf> {
    roots
        .ordered
        .iter()
        .filter(|root| root.space == ItemSpace::Bundle)
        .filter_map(|root| root.ai_root.parent().map(Path::to_path_buf))
        .find(|bundle_root| source_path.starts_with(bundle_root))
}

/// Walk up from `path` to find the first ancestor containing `.ai/`.
fn find_bundle_root(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    loop {
        if current.join(crate::AI_DIR).is_dir() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn find_qualified_target_bundle(
    target_name: &str,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
) -> Result<(PathBuf, MinimalBundleManifest), EngineError> {
    let mut searched = Vec::new();
    let mut skipped = Vec::new();
    let mut matches = Vec::new();

    for root in roots
        .ordered
        .iter()
        .filter(|root| root.space == ItemSpace::Bundle)
    {
        let Some(bundle_root) = root.ai_root.parent().map(Path::to_path_buf) else {
            continue;
        };
        searched.push(bundle_root.display().to_string());
        // Skip bundles whose signed manifest doesn't verify: an unverifiable
        // manifest can't be a trusted target, and one malformed/unsigned bundle
        // in the registered set must not break qualified resolution of others.
        // The reason is retained so a broken registration surfaces in the
        // not-found diagnostic rather than looking like a genuine absence.
        match load_minimal_bundle_manifest(&bundle_root, trust_store) {
            Ok(manifest) if manifest.name == target_name => {
                matches.push((bundle_root, manifest));
            }
            Ok(_) => {}
            Err(reason) => skipped.push(reason.to_string()),
        }
    }

    match matches.len() {
        0 => Err(EngineError::QualifiedBinBundleNotFound {
            bundle: target_name.to_string(),
            searched,
            skipped,
        }),
        1 => Ok(matches.pop().expect("len checked above")),
        _ => Err(EngineError::QualifiedBinBundleAmbiguous {
            bundle: target_name.to_string(),
            roots: matches.into_iter().map(|(root, _)| root).collect(),
        }),
    }
}

/// The subset of a signed bundle manifest that qualified binary resolution
/// needs: the bundle identity and its kind-dependency surface. Intentionally a
/// lenient projection (no `deny_unknown_fields`) so it does not break when the
/// full manifest schema gains fields — the manifest is already signature- and
/// trust-verified before it is parsed here, so extra fields are not a hazard.
#[derive(Debug, Deserialize)]
struct MinimalBundleManifest {
    name: String,
    #[serde(default)]
    provides_kinds: Vec<String>,
    #[serde(default)]
    requires_kinds: Vec<String>,
    #[serde(default)]
    uses_kinds: Vec<String>,
}

fn load_minimal_bundle_manifest(
    bundle_root: &Path,
    trust_store: &TrustStore,
) -> Result<MinimalBundleManifest, EngineError> {
    let manifest_path = bundle_root.join(crate::AI_DIR).join("manifest.yaml");
    let file_type = std::fs::symlink_metadata(&manifest_path)
        .map_err(|e| EngineError::QualifiedBinManifestInvalid {
            path: manifest_path.display().to_string(),
            reason: format!("stat failed: {e}"),
        })?
        .file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        return Err(EngineError::QualifiedBinManifestInvalid {
            path: manifest_path.display().to_string(),
            reason: "manifest is not a regular file".into(),
        });
    }

    let raw = std::fs::read_to_string(&manifest_path).map_err(|e| {
        EngineError::QualifiedBinManifestInvalid {
            path: manifest_path.display().to_string(),
            reason: format!("read failed: {e}"),
        }
    })?;
    let envelope = SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    };
    let sig_header =
        crate::item_resolution::parse_signature_header(&raw, &envelope).ok_or_else(|| {
            EngineError::QualifiedBinManifestInvalid {
                path: manifest_path.display().to_string(),
                reason: "missing or malformed signature header".into(),
            }
        })?;
    let (trust_class, _) =
        crate::trust::verify_item_signature(&raw, &sig_header, &envelope, trust_store).map_err(
            |e| EngineError::QualifiedBinManifestInvalid {
                path: manifest_path.display().to_string(),
                reason: format!("signature verification failed: {e}"),
            },
        )?;
    if trust_class != ContractTrustClass::Trusted {
        return Err(EngineError::QualifiedBinManifestInvalid {
            path: manifest_path.display().to_string(),
            reason: format!(
                "manifest signer {} is not trusted (trust_class: {:?})",
                sig_header.signer_fingerprint, trust_class
            ),
        });
    }

    let body = lillux::signature::strip_signature_lines(&raw);
    serde_yaml::from_str(&body).map_err(|e| EngineError::QualifiedBinManifestInvalid {
        path: manifest_path.display().to_string(),
        reason: format!("parse failed: {e}"),
    })
}

fn ensure_qualified_binary_dependency(
    source: &MinimalBundleManifest,
    target: &MinimalBundleManifest,
) -> Result<(), EngineError> {
    if source.name == target.name {
        return Ok(());
    }
    let source_needs = source.requires_kinds.iter().chain(source.uses_kinds.iter());
    if source_needs.clone().any(|kind| {
        target
            .provides_kinds
            .iter()
            .any(|provided| provided == kind)
    }) {
        return Ok(());
    }

    Err(EngineError::QualifiedBinDependencyMissing {
        source_bundle: source.name.clone(),
        target_bundle: target.name.clone(),
    })
}

fn validate_bundle_name(name: &str, raw_ref: &str) -> Result<(), EngineError> {
    if name.is_empty() {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "no bundle name in qualified binary ref".into(),
        });
    }
    if name.contains('/') || name.contains("..") || name.starts_with('.') || name.contains(' ') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "bundle name must be a single non-hidden identifier without spaces, slashes, or `..`".into(),
        });
    }
    if name.chars().any(|c| c.is_control() || c == '\0') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "bundle name must not contain control characters".into(),
        });
    }
    Ok(())
}

fn verify_item_source_sidecar(
    bin_name: &str,
    bin_path: &Path,
    expected_item_ref: &str,
    item_source: &serde_json::Value,
    trusted_verifying_key: &impl Fn(&str) -> Option<VerifyingKey>,
) -> Result<String, EngineError> {
    let sidecar_path = bin_path.with_file_name(format!("{bin_name}.item_source.json"));
    let signed =
        std::fs::read_to_string(&sidecar_path).map_err(|e| EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: format!("read {}: {e}", sidecar_path.display()),
        })?;

    let header = signed
        .lines()
        .next()
        .and_then(|line| lillux::signature::parse_signature_line(line, "#", None))
        .ok_or_else(|| EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: format!(
                "missing or malformed signature line in {}",
                sidecar_path.display()
            ),
        })?;

    let Some(verifying_key) = trusted_verifying_key(&header.signer_fingerprint) else {
        return Err(EngineError::BinUntrusted {
            bin: bin_name.to_string(),
            fingerprint: header.signer_fingerprint,
        });
    };

    let body = lillux::signature::strip_signature_lines_with_envelope(&signed, "#", None);
    if !lillux::signature::is_valid_signature_for(
        &header.content_hash,
        &header.signature_b64,
        &header.signer_fingerprint,
        &body,
        &verifying_key,
        &header.signer_fingerprint,
    ) {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: "signature verification failed".to_string(),
        });
    }

    let canonical_item_source = lillux::cas::canonical_json(item_source).map_err(|error| {
        EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: format!("item_source cannot be canonicalized: {error}"),
        }
    })?;
    if body != canonical_item_source {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: "signed sidecar body does not match CAS item_source object".to_string(),
        });
    }

    let actual_item_ref = item_source
        .get("item_ref")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if actual_item_ref != expected_item_ref {
        return Err(EngineError::BinSidecarInvalid {
            bin: bin_name.to_string(),
            reason: format!(
                "item_source item_ref `{actual_item_ref}` does not match resolved binary ref `{expected_item_ref}`"
            ),
        });
    }

    Ok(header.signer_fingerprint)
}

/// Validate a binary name for both `bin:<name>` and `bin/<triple>/<name>`.
///
/// Rejects:
///   - empty
///   - any `/`
///   - any `..` segment
///   - leading `.` (hidden file)
///   - any control char or NUL
///   - spaces
fn validate_bin_name(name: &str, raw_ref: &str) -> Result<(), EngineError> {
    if name.is_empty() {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "no binary name after prefix".into(),
        });
    }
    if name.contains('/') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "binary name must not contain slashes".into(),
        });
    }
    if name.contains("..") {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "binary name must not contain path traversal (`..`)".into(),
        });
    }
    if name.starts_with('.') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "binary name must not start with `.` (hidden file)".into(),
        });
    }
    if name.contains(' ') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "binary name must not contain spaces — put subcommand args in the YAML's `args` list".into(),
        });
    }
    if name.chars().any(|c| c.is_control() || c == '\0') {
        return Err(EngineError::InvalidBinPrefix {
            raw: raw_ref.to_string(),
            detail: "binary name must not contain control characters".into(),
        });
    }
    Ok(())
}

/// Decide whether the trust class returned by
/// [`verify_executor_trust`] is high enough to dispatch the binary.
///
/// Both `TrustedBundle` and `TrustedProject` are dispatchable. The
/// effective tier is already the `min` of the raw binary signature
/// trust (which `verify_executor_trust` produces only as
/// `TrustedBundle` / `UntrustedProject` / `Unsigned`) and the
/// descriptor's `root_trust_class` (widened to `TrustedBundle` or
/// `TrustedProject` by `plan_builder::widen_root_trust_class`).
/// A `TrustedProject` here therefore means a system-signed binary
/// reached through a user/project-tier descriptor — safe to run.
/// Anything weaker (`UntrustedProject`, `Unsigned`) must be refused.
fn is_dispatchable_trust_class(tc: TrustClass) -> bool {
    matches!(tc, TrustClass::TrustedBundle | TrustClass::TrustedProject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor_resolution::verify_executor_trust;
    use crate::item_resolution::{ResolutionRoot, ResolutionRoots};
    use crate::trust::{TrustStore, TrustedSigner};
    use lillux::crypto::SigningKey;
    use serde_json::json;

    /// Descriptor=System, binary signed by a system-trusted key.
    /// Effective tier = TrustedBundle; gate accepts.
    #[test]
    fn descriptor_system_binary_system_dispatches_as_system() {
        let item_source = json!({
            "signature_info": { "fingerprint": "sys-fp" }
        });
        let (tc, fp) =
            verify_executor_trust(&item_source, |f| f == "sys-fp", TrustClass::TrustedBundle);
        assert_eq!(tc, TrustClass::TrustedBundle);
        assert_eq!(fp.as_deref(), Some("sys-fp"));
        assert!(is_dispatchable_trust_class(tc));
    }

    /// Descriptor=System, binary signed by a key absent from the trust
    /// store (modeling an untrusted signer under a bundle root). The
    /// effective tier collapses to UntrustedProject and the gate refuses.
    ///
    /// Note: `verify_executor_trust` does not currently model a raw
    /// `TrustedProject` signer tier — its `raw_trust` is only TrustedBundle,
    /// UntrustedProject, or Unsigned. So a "binary signed by a user-tier
    /// signer" is equivalent to "binary signed by a key not in the trust
    /// store" today; this test covers that.
    #[test]
    fn descriptor_system_unknown_signer_refused() {
        let item_source = json!({
            "signature_info": { "fingerprint": "user-fp" }
        });
        let (tc, fp) = verify_executor_trust(&item_source, |_| false, TrustClass::TrustedBundle);
        assert_eq!(tc, TrustClass::UntrustedProject);
        assert_eq!(fp.as_deref(), Some("user-fp"));
        assert!(!is_dispatchable_trust_class(tc));
    }

    /// Descriptor=User, binary signed by a system-trusted key.
    /// Effective tier = TrustedProject (capped by descriptor); gate accepts.
    ///
    /// This is the case the wave-5 oracle audit flagged: previously the
    /// gate hardcoded acceptance to `TrustedBundle` only, so this case
    /// — which is the *normal* dispatch path for any user-tier descriptor
    /// invoking a system-shipped runtime/handler binary — was rejected.
    #[test]
    fn descriptor_user_binary_system_dispatches_as_user() {
        let item_source = json!({
            "signature_info": { "fingerprint": "sys-fp" }
        });
        let (tc, fp) =
            verify_executor_trust(&item_source, |f| f == "sys-fp", TrustClass::TrustedProject);
        assert_eq!(tc, TrustClass::TrustedProject);
        assert_eq!(fp.as_deref(), Some("sys-fp"));
        assert!(is_dispatchable_trust_class(tc));
    }

    /// Descriptor=User, binary signed by an unknown signer.
    /// Effective tier = UntrustedProject; gate refuses.
    #[test]
    fn descriptor_user_unknown_signer_refused() {
        let item_source = json!({
            "signature_info": { "fingerprint": "stranger-fp" }
        });
        let (tc, fp) = verify_executor_trust(&item_source, |_| false, TrustClass::TrustedProject);
        assert_eq!(tc, TrustClass::UntrustedProject);
        assert_eq!(fp.as_deref(), Some("stranger-fp"));
        assert!(!is_dispatchable_trust_class(tc));
    }

    /// Sanity floor: anything below TrustedProject must never dispatch.
    #[test]
    fn untrusted_and_unsigned_are_never_dispatchable() {
        assert!(!is_dispatchable_trust_class(TrustClass::UntrustedProject));
        assert!(!is_dispatchable_trust_class(TrustClass::Unsigned));
    }

    /// Build a minimally valid bundle in `bundle_root` containing a
    /// single binary named `bin_name`, its CAS-stored item_source/manifest,
    /// and the `refs/bundles/manifest` pointer. Returns the signer
    /// fingerprint and signing key used for the signed item-source sidecar.
    fn write_resolver_fixture(bundle_root: &Path, bin_name: &str) -> (String, SigningKey) {
        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        let ai = bundle_root.join(crate::AI_DIR);
        let bin_dir = ai.join("bin").join(triple);
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin_path = bin_dir.join(bin_name);
        let bin_bytes = b"placeholder-binary\n";
        std::fs::write(&bin_path, bin_bytes).unwrap();
        let content_blob_hash = lillux::sha256_hex(bin_bytes);

        let cas = lillux::cas::CasStore::new(ai.join("objects"));
        let signing_key = SigningKey::from_bytes(&[31u8; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&signing_key.verifying_key());
        let item_ref = format!("bin/{triple}/{bin_name}");
        let item_source = serde_json::json!({
            "item_ref": item_ref,
            "content_blob_hash": content_blob_hash,
            "integrity": format!("sha256:{content_blob_hash}"),
            "signature_info": { "fingerprint": fingerprint },
            "mode": 0o644,
        });
        let item_source_hash = cas.store_object(&item_source).unwrap();
        let sidecar_body = lillux::cas::canonical_json(&item_source).unwrap();
        let signed_sidecar =
            lillux::signature::sign_content(&sidecar_body, &signing_key, "#", None);
        std::fs::write(
            bin_path.with_file_name(format!("{bin_name}.item_source.json")),
            signed_sidecar,
        )
        .unwrap();
        let manifest = serde_json::json!({
            "item_source_hashes": {
                item_ref: item_source_hash
            }
        });
        let manifest_hash = cas.store_object(&manifest).unwrap();

        let ref_path = ai.join("refs").join("bundles").join("manifest");
        std::fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
        std::fs::write(ref_path, manifest_hash).unwrap();

        (fingerprint, signing_key)
    }

    fn trusted_key_for<'a>(
        expected_fp: &'a str,
        key: &'a SigningKey,
    ) -> impl Fn(&str) -> Option<lillux::crypto::VerifyingKey> + 'a {
        move |fp| (fp == expected_fp).then(|| key.verifying_key())
    }

    fn trust_store_for(fp: &str, key: &SigningKey) -> TrustStore {
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.to_string(),
            verifying_key: key.verifying_key(),
            label: None,
        }])
    }

    fn write_signed_bundle_manifest(
        bundle_root: &Path,
        name: &str,
        provides_kinds: &[&str],
        requires_kinds: &[&str],
        uses_kinds: &[&str],
        key: &SigningKey,
    ) {
        let ai = bundle_root.join(crate::AI_DIR);
        std::fs::create_dir_all(&ai).unwrap();
        let yaml = format!(
            "name: {name}\nversion: \"0.1.0\"\nprovides_kinds:\n{}requires_kinds:\n{}uses_kinds:\n{}\n",
            yaml_list(provides_kinds),
            yaml_list(requires_kinds),
            yaml_list(uses_kinds),
        );
        let signed = lillux::signature::sign_content(&yaml, key, "#", None);
        std::fs::write(ai.join("manifest.yaml"), signed).unwrap();
    }

    fn yaml_list(values: &[&str]) -> String {
        if values.is_empty() {
            "  []\n".to_string()
        } else {
            values
                .iter()
                .map(|value| format!("  - {value}\n"))
                .collect()
        }
    }

    fn roots_for(bundle_roots: &[&Path]) -> ResolutionRoots {
        ResolutionRoots {
            ordered: bundle_roots
                .iter()
                .enumerate()
                .map(|(i, root)| ResolutionRoot {
                    space: ItemSpace::Bundle,
                    label: format!("bundle:{i}"),
                    ai_root: root.join(crate::AI_DIR),
                })
                .collect(),
        }
    }

    /// `bin/{triple}/<name>` resolves identically to the canonical
    /// `bin/<host-triple>/<name>` shape, including manifest lookup.
    #[test]
    fn placeholder_triple_resolves_against_host_triple() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fp, key) = write_resolver_fixture(&bundle, "demo");

        let resolved = resolve_bundle_binary_ref(
            "bin/{triple}/demo",
            &bundle,
            trusted_key_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("placeholder triple ref must resolve");

        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        assert!(resolved
            .absolute_path
            .ends_with(format!("{}/bin/{triple}/demo", crate::AI_DIR)));
        assert_eq!(resolved.signer_fingerprint, fp);
    }

    /// `bin:<name>` and `bin/{triple}/<name>` must agree on the
    /// resolved path so descriptors can pick whichever shape reads
    /// best without changing the verified target.
    #[test]
    fn short_form_and_placeholder_form_resolve_to_same_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fp, key) = write_resolver_fixture(&bundle, "demo");

        let short = resolve_bundle_binary_ref(
            "bin:demo",
            &bundle,
            trusted_key_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("short form must resolve");
        let placeholder = resolve_bundle_binary_ref(
            "bin/{triple}/demo",
            &bundle,
            trusted_key_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("placeholder form must resolve");

        assert_eq!(short.absolute_path, placeholder.absolute_path);
        assert_eq!(short.manifest_hash, placeholder.manifest_hash);
    }

    #[test]
    fn qualified_runtime_ref_resolves_target_bundle_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let target = tmp.path().join("core");
        let (fp, key) = write_resolver_fixture(&target, "ryeos-core-tools");
        write_signed_bundle_manifest(&source, "authoring", &[], &["tool"], &[], &key);
        write_signed_bundle_manifest(&target, "core", &["tool"], &[], &[], &key);
        let roots = roots_for(&[&source, &target]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");
        std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        std::fs::write(&wrapper, "version: 0.1.0\n").unwrap();

        let resolved = resolve_runtime_binary_command_ref(
            "bin:core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("qualified core binary ref should resolve");

        assert!(resolved
            .absolute_path
            .starts_with(target.join(crate::AI_DIR).join("bin")));
        assert_eq!(resolved.signer_fingerprint, fp);
    }

    #[test]
    fn qualified_runtime_ref_requires_source_dependency_on_target() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let target = tmp.path().join("core");
        let (fp, key) = write_resolver_fixture(&target, "ryeos-core-tools");
        write_signed_bundle_manifest(&source, "authoring", &[], &["knowledge"], &[], &key);
        write_signed_bundle_manifest(&target, "core", &["tool"], &[], &[], &key);
        let roots = roots_for(&[&source, &target]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");

        let err = resolve_runtime_binary_command_ref(
            "bin:core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            EngineError::QualifiedBinDependencyMissing {
                source_bundle,
                target_bundle,
            } if source_bundle == "authoring" && target_bundle == "core"
        ));
    }

    #[test]
    fn qualified_runtime_ref_rejects_slashy_bundle_or_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let (fp, key) = write_resolver_fixture(&source, "local");
        write_signed_bundle_manifest(&source, "authoring", &["tool"], &[], &[], &key);
        let roots = roots_for(&[&source]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");

        let err = resolve_runtime_binary_command_ref(
            "bin:ryeos/core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .unwrap_err();

        assert!(
            matches!(err, EngineError::InvalidBinPrefix { ref detail, .. }
                if detail.contains("bin:<bundle>/<binary>")),
            "expected clear qualified ref parse error, got: {err:?}"
        );
    }

    #[test]
    fn qualified_runtime_ref_rejects_duplicate_target_bundle_names() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let target_a = tmp.path().join("core-a");
        let target_b = tmp.path().join("core-b");
        let (fp, key) = write_resolver_fixture(&target_a, "ryeos-core-tools");
        write_resolver_fixture(&target_b, "ryeos-core-tools");
        write_signed_bundle_manifest(&source, "authoring", &[], &["tool"], &[], &key);
        write_signed_bundle_manifest(&target_a, "core", &["tool"], &[], &[], &key);
        write_signed_bundle_manifest(&target_b, "core", &["tool"], &[], &[], &key);
        let roots = roots_for(&[&source, &target_a, &target_b]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");

        let err = resolve_runtime_binary_command_ref(
            "bin:core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            EngineError::QualifiedBinBundleAmbiguous { bundle, roots }
                if bundle == "core" && roots.len() == 2
        ));
    }

    #[test]
    fn unqualified_runtime_ref_resolves_wrapper_local() {
        // Regression guard for the changed hot path: an unqualified `bin:<name>`
        // still resolves in the wrapper item's own bundle.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let (fp, key) = write_resolver_fixture(&source, "local-tool");
        write_signed_bundle_manifest(&source, "authoring", &[], &[], &[], &key);
        let roots = roots_for(&[&source]);
        let wrapper = source.join(crate::AI_DIR).join("tools/authoring/w.yaml");
        std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        std::fs::write(&wrapper, "version: 0.1.0\n").unwrap();

        let resolved = resolve_runtime_binary_command_ref(
            "bin:local-tool",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("unqualified ref resolves in the wrapper's own bundle");
        assert!(resolved
            .absolute_path
            .starts_with(source.join(crate::AI_DIR).join("bin")));
    }

    #[test]
    fn qualified_runtime_ref_rejects_untrusted_target_manifest() {
        // The target bundle's manifest is signed by a key absent from the trust
        // store, so it is not a trusted candidate — no trusted `core` is found.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let target = tmp.path().join("core");
        let (fp, key) = write_resolver_fixture(&target, "ryeos-core-tools");
        write_signed_bundle_manifest(&source, "authoring", &[], &["tool"], &[], &key);
        let rogue = SigningKey::from_bytes(&[7u8; 32]);
        write_signed_bundle_manifest(&target, "core", &["tool"], &[], &[], &rogue);
        let roots = roots_for(&[&source, &target]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");

        let err = resolve_runtime_binary_command_ref(
            "bin:core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .unwrap_err();
        // Not a trusted candidate → not found, and the broken registration is
        // surfaced in the diagnostic rather than looking like a genuine absence.
        match err {
            EngineError::QualifiedBinBundleNotFound {
                bundle, skipped, ..
            } => {
                assert_eq!(bundle, "core");
                assert!(
                    skipped.iter().any(|s| s.contains("invalid")),
                    "expected the skipped invalid manifest to be reported, got: {skipped:?}"
                );
            }
            other => panic!("expected QualifiedBinBundleNotFound, got: {other:?}"),
        }
    }

    #[test]
    fn qualified_ref_tolerates_full_signed_manifest_fields() {
        // Guards the intentional lenient projection: a real generated manifest
        // carries `description` and a `runtime_authority` block (and may gain
        // more fields). The minimal projection must ignore them, not reject the
        // manifest — so qualified resolution keeps working against real bundles.
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("authoring");
        let target = tmp.path().join("core");
        let (fp, key) = write_resolver_fixture(&target, "ryeos-core-tools");
        write_full_signed_manifest(
            &source,
            "authoring",
            &[],
            &["tool"],
            &key,
            "runtime_authority:\n  item_authoring:\n    - kind: knowledge\n      namespace: runtime-authored/*\n",
        );
        write_full_signed_manifest(
            &target,
            "core",
            &["tool"],
            &[],
            &key,
            "runtime_authority: {}\n",
        );
        let roots = roots_for(&[&source, &target]);
        let wrapper = source
            .join(crate::AI_DIR)
            .join("tools/authoring/author-item.yaml");

        let resolved = resolve_runtime_binary_command_ref(
            "bin:core/ryeos-core-tools",
            &wrapper,
            &roots,
            &trust_store_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .expect("full-shape signed manifests must still resolve");
        assert!(resolved
            .absolute_path
            .starts_with(target.join(crate::AI_DIR).join("bin")));
    }

    /// Like `write_signed_bundle_manifest` but includes `description` and a
    /// caller-supplied trailing block (e.g. `runtime_authority`), mirroring a
    /// generated `.ai/manifest.yaml`.
    fn write_full_signed_manifest(
        bundle_root: &Path,
        name: &str,
        provides_kinds: &[&str],
        requires_kinds: &[&str],
        key: &SigningKey,
        extra_yaml: &str,
    ) {
        let ai = bundle_root.join(crate::AI_DIR);
        std::fs::create_dir_all(&ai).unwrap();
        let yaml = format!(
            "name: {name}\nversion: \"0.1.0\"\ndescription: \"full manifest\"\nprovides_kinds:\n{}requires_kinds:\n{}uses_kinds:\n{}{extra_yaml}",
            yaml_list(provides_kinds),
            yaml_list(requires_kinds),
            yaml_list(&[]),
        );
        let signed = lillux::signature::sign_content(&yaml, key, "#", None);
        std::fs::write(ai.join("manifest.yaml"), signed).unwrap();
    }

    #[test]
    fn forged_manifest_with_trusted_fingerprint_rejected_without_matching_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let (fp, key) = write_resolver_fixture(&bundle, "demo");
        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        let ai = bundle.join(crate::AI_DIR);
        let bin_path = ai.join("bin").join(triple).join("demo");

        let forged_bytes = b"forged-binary\n";
        std::fs::write(&bin_path, forged_bytes).unwrap();
        let forged_hash = lillux::sha256_hex(forged_bytes);
        let forged_item_source = serde_json::json!({
            "item_ref": format!("bin/{triple}/demo"),
            "content_blob_hash": forged_hash,
            "integrity": format!("sha256:{forged_hash}"),
            "signature_info": { "fingerprint": fp },
            "mode": 0o644,
        });
        let cas = lillux::cas::CasStore::new(ai.join("objects"));
        let forged_item_source_hash = cas.store_object(&forged_item_source).unwrap();
        let forged_manifest = serde_json::json!({
            "item_source_hashes": {
                format!("bin/{triple}/demo"): forged_item_source_hash
            }
        });
        let forged_manifest_hash = cas.store_object(&forged_manifest).unwrap();
        std::fs::write(
            ai.join("refs").join("bundles").join("manifest"),
            forged_manifest_hash,
        )
        .unwrap();

        let err = resolve_bundle_binary_ref(
            "bin:demo",
            &bundle,
            trusted_key_for(&fp, &key),
            TrustClass::TrustedBundle,
        )
        .unwrap_err();

        assert!(
            matches!(err, EngineError::BinSidecarInvalid { ref reason, .. }
                if reason.contains("does not match CAS item_source")),
            "expected sidecar/CAS mismatch rejection, got: {err:?}"
        );
    }

    // ── Phase 1A new tests ─────────────────────────────────────────

    #[test]
    fn short_form_rejects_traversal() {
        let err = resolve_bundle_binary_ref(
            "bin:../demo",
            Path::new("/tmp/bundle"),
            |_| None,
            TrustClass::TrustedBundle,
        )
        .unwrap_err();
        // `..` contains no `/` in the `bin:../demo` name (`../demo` does
        // contain `/` though), so the rejection depends on order: the
        // slash check fires first. Either slash or path traversal is fine.
        assert!(
            matches!(err, EngineError::InvalidBinPrefix { ref detail, .. }
                if detail.contains("path traversal") || detail.contains("slashes")),
            "expected path traversal or slash rejection, got: {err:?}"
        );
    }

    #[test]
    fn short_form_rejects_slash() {
        let err = resolve_bundle_binary_ref(
            "bin:subdir/demo",
            Path::new("/tmp/bundle"),
            |_| None,
            TrustClass::TrustedBundle,
        )
        .unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidBinPrefix { ref detail, .. } if detail.contains("slashes")),
            "expected slash rejection, got: {err:?}"
        );
    }

    #[test]
    fn path_form_rejects_empty_name() {
        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        let err = resolve_bundle_binary_ref(
            &format!("bin/{triple}/"),
            Path::new("/tmp/bundle"),
            |_| None,
            TrustClass::TrustedBundle,
        )
        .unwrap_err();
        // splitn(4, '/') on "bin/x86_64-unknown-linux-gnu/" gives
        // ["bin", "x86_64...", ""], so name is empty.
        assert!(
            matches!(err, EngineError::InvalidBinPrefix { ref detail, .. } if detail.contains("no binary name")),
            "expected empty name rejection, got: {err:?}"
        );
    }

    #[test]
    fn symlink_escaping_bundle_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        let bin_dir = bundle.join(crate::AI_DIR).join("bin").join(triple);
        std::fs::create_dir_all(&bin_dir).unwrap();

        // Place the real binary outside the bundle.
        let outside = tmp.path().join("outside-binary");
        std::fs::write(&outside, b"evil").unwrap();

        // Create a symlink inside the bin dir pointing outside.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside, bin_dir.join("escaped")).unwrap();
        }

        // Build a manifest that includes the symlink target hash.
        let bin_bytes = b"evil";
        let content_blob_hash = lillux::sha256_hex(bin_bytes);
        let cas = lillux::cas::CasStore::new(bundle.join(crate::AI_DIR).join("objects"));
        let item_source = serde_json::json!({
            "content_blob_hash": content_blob_hash,
            "signature_info": { "fingerprint": "test-fp" }
        });
        let item_source_hash = cas.store_object(&item_source).unwrap();
        let manifest = serde_json::json!({
            "item_source_hashes": {
                format!("bin/{triple}/escaped"): item_source_hash
            }
        });
        let manifest_hash = cas.store_object(&manifest).unwrap();
        let ref_path = bundle
            .join(crate::AI_DIR)
            .join("refs")
            .join("bundles")
            .join("manifest");
        std::fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
        std::fs::write(&ref_path, manifest_hash).unwrap();

        let err =
            resolve_bundle_binary_ref("bin:escaped", &bundle, |_| None, TrustClass::TrustedBundle)
                .unwrap_err();

        // The symlink resolves outside the bundle bin dir.
        // Either BinOutsideBundle or BinNotRegularFile is acceptable
        // depending on whether the outside target is a regular file.
        // Since the outside file IS a regular file, we expect
        // BinOutsideBundle.
        assert!(
            matches!(err, EngineError::BinOutsideBundle { .. }),
            "expected BinOutsideBundle for symlink escaping bundle, got: {err:?}"
        );
    }

    #[test]
    fn non_regular_file_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
        let bin_dir = bundle.join(crate::AI_DIR).join("bin").join(triple);
        std::fs::create_dir_all(&bin_dir).unwrap();

        // Create a directory where a binary would go.
        let dir_path = bin_dir.join("not-a-file");
        std::fs::create_dir(&dir_path).unwrap();

        let err = resolve_bundle_binary_ref(
            "bin:not-a-file",
            &bundle,
            |_| None,
            TrustClass::TrustedBundle,
        )
        .unwrap_err();

        // A directory is not a regular file.
        assert!(
            matches!(err, EngineError::BinNotRegularFile { .. })
                || matches!(err, EngineError::BinNotFound { .. }),
            "expected BinNotRegularFile or BinNotFound for directory, got: {err:?}"
        );
    }
}
