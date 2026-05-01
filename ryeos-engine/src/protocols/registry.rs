//! Protocol registry — loads protocol descriptors from base roots,
//! verifies signatures, validates vocabulary invariants, and provides
//! lookup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::protocol_vocabulary::{
    is_compatible_lifecycle_detached, is_compatible_shape_mode, validate_env_name,
    CallbackChannel, EnvInjectionSource,
};
use crate::protocols::descriptor::ProtocolDescriptor;
use crate::protocols::SUPPORTED_PROTOCOL_ABI_VERSION;
use crate::resolution::TrustClass;
use crate::trust::TrustStore;
use crate::AI_DIR;

/// A protocol descriptor that has been loaded, verified, and validated.
#[derive(Debug, Clone)]
pub struct VerifiedProtocol {
    pub canonical_ref: String,
    pub descriptor: ProtocolDescriptor,
    pub trust_class: TrustClass,
    pub bundle_root: PathBuf,
    pub descriptor_path: PathBuf,
}

/// Registry of protocol descriptors loaded from base roots (system +
/// optional user). NO project overlay.
#[derive(Debug, Clone)]
pub struct ProtocolRegistry {
    /// canonical_ref → entry
    entries: HashMap<String, VerifiedProtocol>,
    /// SHA-256 of (sorted canonical refs ++ abi_versions ++
    /// content hashes); contributes to engine composite fingerprint.
    fingerprint: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("protocol `{path}`: failed to read: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("protocol `{path}`: signature verification failed: {detail}")]
    SignatureInvalid { path: PathBuf, detail: String },
    #[error("protocol `{path}`: malformed YAML: {detail}")]
    MalformedYaml { path: PathBuf, detail: String },
    #[error("protocol `{path}`: kind discriminator must be `protocol`, got `{found}`")]
    WrongKindDiscriminator { path: PathBuf, found: String },
    #[error("protocol `{name}` declares abi_version `{found}` but engine supports `{expected}`")]
    AbiVersionMismatch {
        name: String,
        expected: String,
        found: String,
    },
    #[error("protocol `{name}`: descriptor file stem must match `name` field")]
    NameFilenameMismatch { name: String, file_stem: String },
    #[error("protocol `{name}`: vocabulary error: {source}")]
    Vocabulary {
        name: String,
        #[source]
        source: crate::protocol_vocabulary::VocabularyError,
    },
    #[error("duplicate protocol ref `{canonical_ref}` found in:\n  {paths:?}")]
    DuplicateRef {
        canonical_ref: String,
        paths: Vec<PathBuf>,
    },
    #[error("protocol `{canonical_ref}` not registered")]
    NotRegistered { canonical_ref: String },
}

impl ProtocolRegistry {
    /// Load protocol descriptors from the given base roots (system
    /// bundles + optional user root). NO project overlay.
    ///
    /// Each root is scanned for `<root>/<AI_DIR>/protocols/**/*.yaml`.
    /// Descriptors must be signed; the signature is verified against
    /// the trust store.
    pub fn load_base(
        roots: &[PathBuf],
        trust_store: &TrustStore,
    ) -> Result<Self, ProtocolError> {
        let mut entries: HashMap<String, VerifiedProtocol> = HashMap::new();
        let mut fingerprint_parts: Vec<String> = Vec::new();

        for root in roots {
            let protocols_dir = root.join(AI_DIR).join("protocols");
            if !protocols_dir.is_dir() {
                continue;
            }
            let mut yaml_paths = Vec::new();
            collect_yaml_paths(&protocols_dir, &mut yaml_paths)?;
            yaml_paths.sort();

            for path in &yaml_paths {
                let verified = load_and_verify_protocol(path, root, trust_store)?;

                if let Some(existing) = entries.get(&verified.canonical_ref) {
                    return Err(ProtocolError::DuplicateRef {
                        canonical_ref: verified.canonical_ref.clone(),
                        paths: vec![existing.descriptor_path.clone(), path.clone()],
                    });
                }

                fingerprint_parts.push(format!(
                    "{}|{}|{}",
                    verified.canonical_ref,
                    verified.descriptor.abi_version,
                    // Use content hash from the signature verification step.
                    // We'll compute it inline here.
                    lillux::signature::content_hash(
                        &lillux::signature::strip_signature_lines(
                            &std::fs::read_to_string(path).map_err(|e| ProtocolError::Io {
                                path: path.clone(),
                                source: e,
                            })?,
                        ),
                    ),
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

    /// Look up a protocol by canonical ref.
    pub fn get(&self, canonical_ref: &str) -> Option<&VerifiedProtocol> {
        self.entries.get(canonical_ref)
    }

    /// Iterate all registered protocols.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &VerifiedProtocol)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// The registry's contribution to the engine composite fingerprint.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Look up a protocol; return NotRegistered if missing.
    pub fn require(
        &self,
        canonical_ref: &str,
    ) -> Result<&VerifiedProtocol, ProtocolError> {
        self.entries
            .get(canonical_ref)
            .ok_or_else(|| ProtocolError::NotRegistered {
                canonical_ref: canonical_ref.to_owned(),
            })
    }
}

/// Recursively collect `.yaml`/`.yml` file paths.
fn collect_yaml_paths(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), ProtocolError> {
    let entries = std::fs::read_dir(dir).map_err(|e| ProtocolError::Io {
        path: dir.to_owned(),
        source: e,
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| ProtocolError::Io {
            path: dir.to_owned(),
            source: e,
        })?;
        let path = entry.path();
        let ft = entry.file_type().map_err(|e| ProtocolError::Io {
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

/// Load a protocol descriptor from a YAML file: verify signature,
/// parse, validate vocabulary invariants.
fn load_and_verify_protocol(
    yaml_path: &Path,
    bundle_root: &Path,
    trust_store: &TrustStore,
) -> Result<VerifiedProtocol, ProtocolError> {
    let content = std::fs::read_to_string(yaml_path).map_err(|e| ProtocolError::Io {
        path: yaml_path.to_owned(),
        source: e,
    })?;

    // Verify signature envelope.
    let sig_header = lillux::signature::parse_signature_line(
        content.lines().next().unwrap_or(""),
        "#",
        None,
    )
    .ok_or_else(|| ProtocolError::SignatureInvalid {
        path: yaml_path.to_owned(),
        detail: "missing or malformed signature line".to_owned(),
    })?;

    let body = lillux::signature::strip_signature_lines(&content);
    let actual_hash = lillux::signature::content_hash(&body);
    if actual_hash != sig_header.content_hash {
        return Err(ProtocolError::SignatureInvalid {
            path: yaml_path.to_owned(),
            detail: format!(
                "content hash mismatch: signed {} but file hashes to {}",
                sig_header.content_hash, actual_hash
            ),
        });
    }

    let signer = trust_store
        .get(&sig_header.signer_fingerprint)
        .ok_or_else(|| ProtocolError::SignatureInvalid {
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
        return Err(ProtocolError::SignatureInvalid {
            path: yaml_path.to_owned(),
            detail: "Ed25519 signature verification failed".to_owned(),
        });
    }

    // Parse the descriptor.
    let descriptor: ProtocolDescriptor =
        serde_yaml::from_str(&body).map_err(|e| ProtocolError::MalformedYaml {
            path: yaml_path.to_owned(),
            detail: format!("YAML parse error: {e}"),
        })?;

    // Validate invariants.
    validate_protocol_descriptor(yaml_path, &descriptor)?;

    // Derive canonical ref from category + name.
    let canonical_ref = format!("protocol:{}/{}", descriptor.category, descriptor.name);

    Ok(VerifiedProtocol {
        canonical_ref,
        descriptor,
        trust_class: TrustClass::TrustedSystem,
        bundle_root: bundle_root.to_owned(),
        descriptor_path: yaml_path.to_owned(),
    })
}

/// Validate all vocabulary invariants on a protocol descriptor.
fn validate_protocol_descriptor(
    path: &Path,
    desc: &ProtocolDescriptor,
) -> Result<(), ProtocolError> {
    // 1. kind == "protocol"
    if desc.kind != "protocol" {
        return Err(ProtocolError::WrongKindDiscriminator {
            path: path.to_owned(),
            found: desc.kind.clone(),
        });
    }

    // 2. abi_version match
    if desc.abi_version != SUPPORTED_PROTOCOL_ABI_VERSION {
        return Err(ProtocolError::AbiVersionMismatch {
            name: desc.name.clone(),
            expected: SUPPORTED_PROTOCOL_ABI_VERSION.to_owned(),
            found: desc.abi_version.clone(),
        });
    }

    // 3. name matches filename stem
    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if desc.name != file_stem {
        return Err(ProtocolError::NameFilenameMismatch {
            name: desc.name.clone(),
            file_stem: file_stem.to_owned(),
        });
    }

    // 4. category non-empty
    if desc.category.is_empty() {
        return Err(ProtocolError::MalformedYaml {
            path: path.to_owned(),
            detail: "category must be non-empty".to_owned(),
        });
    }

    // 5. (stdout.shape, stdout.mode) compatibility matrix
    is_compatible_shape_mode(desc.stdout.shape, desc.stdout.mode).map_err(|e| {
        ProtocolError::Vocabulary {
            name: desc.name.clone(),
            source: e,
        }
    })?;

    // 6. (lifecycle.mode, capabilities.allows_detached) matrix
    is_compatible_lifecycle_detached(desc.lifecycle.mode, desc.capabilities.allows_detached)
        .map_err(|e| ProtocolError::Vocabulary {
            name: desc.name.clone(),
            source: e,
        })?;

    // 7. Env injection invariants
    let mut seen_names = std::collections::HashSet::new();
    let mut has_callback_injection = false;
    for inj in &desc.env_injections {
        // 7a. valid POSIX name, not reserved
        validate_env_name(&inj.name).map_err(|e| ProtocolError::Vocabulary {
            name: desc.name.clone(),
            source: e,
        })?;
        // 7b. unique within descriptor
        if !seen_names.insert(inj.name.clone()) {
            return Err(ProtocolError::Vocabulary {
                name: desc.name.clone(),
                source: crate::protocol_vocabulary::VocabularyError::DuplicateEnvInjection {
                    name: inj.name.clone(),
                },
            });
        }
        if matches!(inj.source, EnvInjectionSource::CallbackTokenUrl) {
            has_callback_injection = true;
        }
    }

    // 8. callback_channel ↔ callback_token_url injection symmetry
    match desc.callback_channel {
        CallbackChannel::HttpV1 => {
            if !has_callback_injection {
                return Err(ProtocolError::Vocabulary {
                    name: desc.name.clone(),
                    source: crate::protocol_vocabulary::VocabularyError::HttpV1WithoutCallbackInjection,
                });
            }
        }
        CallbackChannel::None => {
            if has_callback_injection {
                return Err(ProtocolError::Vocabulary {
                    name: desc.name.clone(),
                    source: crate::protocol_vocabulary::VocabularyError::CallbackInjectionWithoutChannel {
                        name: desc.env_injections
                            .iter()
                            .find(|i| matches!(i.source, EnvInjectionSource::CallbackTokenUrl))
                            .map(|i| i.name.clone())
                            .unwrap_or_default(),
                    },
                });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_has_fingerprint() {
        let reg = ProtocolRegistry::empty();
        assert_eq!(reg.fingerprint(), "empty");
        assert_eq!(reg.iter().count(), 0);
    }

    #[test]
    fn require_returns_not_registered_for_missing() {
        let reg = ProtocolRegistry::empty();
        let result = reg.require("protocol:rye/core/opaque");
        assert!(result.is_err());
        match result.unwrap_err() {
            ProtocolError::NotRegistered { .. } => {}
            other => panic!("wrong error: {other:?}"),
        }
    }
}
