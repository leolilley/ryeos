//! Handler registry — loads handler descriptors from base roots,
//! verifies signatures, validates invariants, and provides lookup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::binary_resolver::resolve_bundle_binary_ref;
use crate::handlers::descriptor::{HandlerDescriptor, HandlerServes};
use crate::handlers::SUPPORTED_HANDLER_ABI_VERSION;
use crate::resolution::TrustClass;
use crate::trust::TrustStore;
use crate::AI_DIR;

/// A handler descriptor that has been loaded, verified, and had its
/// binary resolved.
#[derive(Debug, Clone)]
pub struct VerifiedHandler {
    pub canonical_ref: String,
    pub descriptor: HandlerDescriptor,
    pub trust_class: TrustClass,
    pub bundle_root: PathBuf,
    pub descriptor_path: PathBuf,
    pub resolved_binary_path: PathBuf,
}

/// Registry of handler descriptors loaded from base roots (system +
/// optional user). NO project overlay.
#[derive(Debug, Clone)]
pub struct HandlerRegistry {
    /// canonical_ref → entry
    entries: HashMap<String, VerifiedHandler>,
    /// SHA-256 of (sorted canonical refs ++ binary content hashes ++
    /// abi_versions); contributes to engine composite fingerprint.
    fingerprint: String,
}

#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    #[error("handler `{path}`: failed to read: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("handler `{path}`: signature verification failed: {detail}")]
    SignatureInvalid { path: PathBuf, detail: String },
    #[error("handler `{path}`: malformed YAML: {detail}")]
    MalformedYaml { path: PathBuf, detail: String },
    #[error("handler `{path}`: kind discriminator must be `handler`, got `{found}`")]
    WrongKindDiscriminator { path: PathBuf, found: String },
    #[error("handler `{name}` declares abi_version `{found}` but engine supports `{expected}`")]
    AbiVersionMismatch {
        name: String,
        expected: String,
        found: String,
    },
    #[error("handler `{name}`: required_caps must be empty (handlers are pure functions)")]
    HandlersMustHaveNoCaps { name: String },
    #[error("handler `{name}`: binary_ref `{found}` does not match `bin/<triple>/<name>` shape")]
    BadBinaryRefShape { name: String, found: String },
    #[error("handler `{name}`: descriptor file stem must match `name` field")]
    NameFilenameMismatch { name: String, file_stem: String },
    #[error("duplicate handler ref `{canonical_ref}` found in:\n  {paths:?}")]
    DuplicateRef {
        canonical_ref: String,
        paths: Vec<PathBuf>,
    },
    #[error("handler `{name}`: binary resolution failed: {detail}")]
    BinaryResolution { name: String, detail: String },
    #[error("handler `{canonical_ref}`: serves `{actual:?}` but caller expected `{expected:?}`")]
    ServesMismatch {
        canonical_ref: String,
        actual: HandlerServes,
        expected: HandlerServes,
    },
    #[error("handler `{canonical_ref}` not registered")]
    NotRegistered { canonical_ref: String },
}

impl HandlerRegistry {
    /// Load handler descriptors from the given base roots (system
    /// bundles + optional user root). NO project overlay.
    ///
    /// Each root is scanned for `<root>/<AI_DIR>/handlers/**/*.yaml`.
    /// Descriptors must be signed; the signature is verified against
    /// the trust store.
    pub fn load_base(
        roots: &[PathBuf],
        trust_store: &TrustStore,
    ) -> Result<Self, HandlerError> {
        let mut entries: HashMap<String, VerifiedHandler> = HashMap::new();
        let mut fingerprint_parts: Vec<String> = Vec::new();

        for root in roots {
            let handlers_dir = root.join(AI_DIR).join("handlers");
            if !handlers_dir.is_dir() {
                continue;
            }
            let mut yaml_paths = Vec::new();
            collect_yaml_paths(&handlers_dir, &mut yaml_paths)?;
            yaml_paths.sort();

            for path in &yaml_paths {
                let verified = load_and_verify_handler(path, root, trust_store)?;

                // Check for duplicates across roots.
                if let Some(existing) = entries.get(&verified.canonical_ref) {
                    return Err(HandlerError::DuplicateRef {
                        canonical_ref: verified.canonical_ref.clone(),
                        paths: vec![existing.descriptor_path.clone(), path.clone()],
                    });
                }

                fingerprint_parts.push(format!(
                    "{}|{}",
                    verified.canonical_ref, verified.descriptor.abi_version
                ));
                entries.insert(verified.canonical_ref.clone(), verified);
            }
        }

        fingerprint_parts.sort();
        let fingerprint = lillux::cas::sha256_hex(fingerprint_parts.join(",").as_bytes());

        Ok(Self {
            entries,
            fingerprint,
        })
    }

    /// Build an empty registry (for testing).
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            fingerprint: "empty".to_owned(),
        }
    }

    /// Look up a handler by canonical ref.
    pub fn get(&self, canonical_ref: &str) -> Option<&VerifiedHandler> {
        self.entries.get(canonical_ref)
    }

    /// Iterate all registered handlers.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &VerifiedHandler)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// The registry's contribution to the engine composite fingerprint.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Look up a handler and verify it serves the expected role.
    pub fn ensure_serves(
        &self,
        canonical_ref: &str,
        expected: HandlerServes,
    ) -> Result<&VerifiedHandler, HandlerError> {
        let handler = self
            .entries
            .get(canonical_ref)
            .ok_or_else(|| HandlerError::NotRegistered {
                canonical_ref: canonical_ref.to_owned(),
            })?;
        if handler.descriptor.serves != expected {
            return Err(HandlerError::ServesMismatch {
                canonical_ref: canonical_ref.to_owned(),
                actual: handler.descriptor.serves,
                expected,
            });
        }
        Ok(handler)
    }
}

/// Recursively collect `.yaml`/`.yml` file paths.
fn collect_yaml_paths(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), HandlerError> {
    let entries = std::fs::read_dir(dir).map_err(|e| HandlerError::Io {
        path: dir.to_owned(),
        source: e,
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| HandlerError::Io {
            path: dir.to_owned(),
            source: e,
        })?;
        let path = entry.path();
        let ft = entry.file_type().map_err(|e| HandlerError::Io {
            path: path.clone(),
            source: e,
        })?;
        if ft.is_dir() {
            collect_yaml_paths(&path, out)?;
        } else if ft.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "yaml" | "yml") {
                out.push(path);
            }
        }
    }
    Ok(())
}

/// Load a handler descriptor from a YAML file: verify signature,
/// parse, validate invariants, resolve binary.
fn load_and_verify_handler(
    yaml_path: &Path,
    bundle_root: &Path,
    trust_store: &TrustStore,
) -> Result<VerifiedHandler, HandlerError> {
    let content = std::fs::read_to_string(yaml_path).map_err(|e| HandlerError::Io {
        path: yaml_path.to_owned(),
        source: e,
    })?;

    // Verify signature envelope.
    let sig_header = lillux::signature::parse_signature_line(
        content.lines().next().unwrap_or(""),
        "#",
        None,
    )
    .ok_or_else(|| HandlerError::SignatureInvalid {
        path: yaml_path.to_owned(),
        detail: "missing or malformed signature line".to_owned(),
    })?;

    let body = lillux::signature::strip_signature_lines(&content);
    let actual_hash = lillux::signature::content_hash(&body);
    if actual_hash != sig_header.content_hash {
        return Err(HandlerError::SignatureInvalid {
            path: yaml_path.to_owned(),
            detail: format!(
                "content hash mismatch: signed {} but file hashes to {}",
                sig_header.content_hash, actual_hash
            ),
        });
    }

    let signer = trust_store
        .get(&sig_header.signer_fingerprint)
        .ok_or_else(|| HandlerError::SignatureInvalid {
            path: yaml_path.to_owned(),
            detail: format!(
                "untrusted signer fingerprint {}",
                sig_header.signer_fingerprint
            ),
        })?;

    if !lillux::signature::verify_signature(
        &sig_header.content_hash,
        &sig_header.signature_b64,
        &signer.verifying_key,
    ) {
        return Err(HandlerError::SignatureInvalid {
            path: yaml_path.to_owned(),
            detail: "Ed25519 signature verification failed".to_owned(),
        });
    }

    // Parse the descriptor.
    let descriptor: HandlerDescriptor =
        serde_yaml::from_str(&body).map_err(|e| HandlerError::MalformedYaml {
            path: yaml_path.to_owned(),
            detail: format!("YAML parse error: {e}"),
        })?;

    // Validate invariants.
    validate_handler_descriptor(yaml_path, &descriptor)?;

    // Resolve binary.
    let resolved = resolve_bundle_binary_ref(
        &descriptor.binary_ref,
        bundle_root,
        |fp| trust_store.get(fp).is_some(),
    )
    .map_err(|e| HandlerError::BinaryResolution {
        name: descriptor.name.clone(),
        detail: format!("{e}"),
    })?;

    // Derive canonical ref from category + name.
    let canonical_ref = format!("handler:{}/{}", descriptor.category, descriptor.name);

    Ok(VerifiedHandler {
        canonical_ref,
        descriptor,
        trust_class: TrustClass::TrustedSystem,
        bundle_root: bundle_root.to_owned(),
        descriptor_path: yaml_path.to_owned(),
        resolved_binary_path: resolved.absolute_path,
    })
}

/// Validate all invariants on a handler descriptor.
fn validate_handler_descriptor(
    path: &Path,
    desc: &HandlerDescriptor,
) -> Result<(), HandlerError> {
    // 1. kind == "handler"
    if desc.kind != "handler" {
        return Err(HandlerError::WrongKindDiscriminator {
            path: path.to_owned(),
            found: desc.kind.clone(),
        });
    }

    // 2. abi_version match
    if desc.abi_version != SUPPORTED_HANDLER_ABI_VERSION {
        return Err(HandlerError::AbiVersionMismatch {
            name: desc.name.clone(),
            expected: SUPPORTED_HANDLER_ABI_VERSION.to_owned(),
            found: desc.abi_version.clone(),
        });
    }

    // 3. required_caps empty
    if !desc.required_caps.is_empty() {
        return Err(HandlerError::HandlersMustHaveNoCaps {
            name: desc.name.clone(),
        });
    }

    // 4. binary_ref matches bin/<triple>/<name> shape
    let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");
    let expected_prefix = format!("bin/{triple}/");
    if !desc.binary_ref.starts_with(&expected_prefix) {
        return Err(HandlerError::BadBinaryRefShape {
            name: desc.name.clone(),
            found: desc.binary_ref.clone(),
        });
    }

    // 5. name matches filename stem
    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if desc.name != file_stem {
        return Err(HandlerError::NameFilenameMismatch {
            name: desc.name.clone(),
            file_stem: file_stem.to_owned(),
        });
    }

    // 6. category non-empty
    if desc.category.is_empty() {
        return Err(HandlerError::MalformedYaml {
            path: path.to_owned(),
            detail: "category must be non-empty".to_owned(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_has_fingerprint() {
        let reg = HandlerRegistry::empty();
        assert_eq!(reg.fingerprint(), "empty");
        assert_eq!(reg.iter().count(), 0);
    }

    #[test]
    fn ensure_serves_returns_not_registered_for_missing() {
        let reg = HandlerRegistry::empty();
        let result = reg.ensure_serves("handler:foo/bar", HandlerServes::Parser);
        assert!(result.is_err());
        match result.unwrap_err() {
            HandlerError::NotRegistered { .. } => {}
            other => panic!("wrong error: {other:?}"),
        }
    }
}
