//! Unified bundle binary resolution for runtimes + handlers.
//!
//! Accepts two intentional cross-consumer ref shapes — neither is a
//! compatibility shim; they're the canonical forms used by different
//! authoring surfaces and the resolver normalizes both to the same
//! verified `<bundle>/.ai/bin/<host_triple>/<name>` path:
//!
//!   - `bin:<name>` — the canonical short form used by tool YAMLs.
//!     The triple is implicit (always the host triple), so authors
//!     don't have to mention it; it's also the form `rye-bundle-tool`
//!     emits when describing tool binary refs.
//!
//!   - `bin/<triple>/<name>` — the path-style form used by runtime
//!     YAMLs and handler descriptors. The triple is explicit so a
//!     bundle can ship multiple architectures side-by-side and the
//!     descriptor unambiguously names which one it covers.
//!
//! Both shapes go through the same manifest-hash + trust-store
//! verification path below.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::EngineError;
use crate::resolution::TrustClass;

/// Result of resolving a binary reference.
pub struct ResolvedBinary {
    pub absolute_path: PathBuf,
    pub manifest_hash: String,
    pub signer_fingerprint: String,
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
    trust_store_has_fingerprint: impl Fn(&str) -> bool,
    root_trust_class: TrustClass,
) -> Result<ResolvedBinary, EngineError> {
    let triple = env!("RYEOS_ENGINE_HOST_TRIPLE");

    // Determine the binary name and item_ref based on ref shape.
    let (bin_name, item_ref, bin_path) = if let Some(name) = binary_ref.strip_prefix("bin:") {
        // Canonical short shape: bin:<name> (triple implicit = host)
        let name = name.trim();
        if name.is_empty() {
            return Err(EngineError::InvalidBinPrefix {
                raw: binary_ref.to_string(),
                detail: "no binary name after `bin:`".into(),
            });
        }
        if name.contains(' ') {
            return Err(EngineError::InvalidBinPrefix {
                raw: binary_ref.to_string(),
                detail: "binary name must not contain spaces — put subcommand args in the YAML's `args` list".into(),
            });
        }
        let path = bundle_root
            .join(crate::AI_DIR)
            .join("bin")
            .join(triple)
            .join(name);
        let iref = format!("bin/{triple}/{name}");
        (name.to_string(), iref, path)
    } else if binary_ref.starts_with("bin/") {
        // Path-style shape: bin/<triple>/<name>
        let parts: Vec<&str> = binary_ref.splitn(4, '/').collect();
        if parts.len() != 3 {
            return Err(EngineError::InvalidBinPrefix {
                raw: binary_ref.to_string(),
                detail: "path-style binary_ref must be `bin/<triple>/<name>`".into(),
            });
        }
        let ref_triple = parts[1];
        let name = parts[2];

        if ref_triple != triple {
            return Err(EngineError::InvalidBinPrefix {
                raw: binary_ref.to_string(),
                detail: format!(
                    "binary_ref triple `{ref_triple}` doesn't match host triple `{triple}`"
                ),
            });
        }

        // Path traversal check.
        if name.contains("..") || name.contains('/') {
            return Err(EngineError::InvalidBinPrefix {
                raw: binary_ref.to_string(),
                detail: "binary name must not contain path traversal or slashes".into(),
            });
        }

        let path = bundle_root
            .join(crate::AI_DIR)
            .join("bin")
            .join(triple)
            .join(name);
        (name.to_string(), binary_ref.to_string(), path)
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
            EngineError::Internal(format!(
                "CAS read error for manifest {manifest_hash}: {e}"
            ))
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

    let item_source_hash = item_source_hashes
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

    let content_blob_hash = item_source
        .get("content_blob_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let bin_bytes = std::fs::read(&bin_path).map_err(|e| {
        EngineError::Internal(format!(
            "failed to read binary {}: {e}",
            bin_path.display()
        ))
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

    let (trust_class, fingerprint) =
        crate::executor_resolution::verify_executor_trust(&item_source, trust_store_has_fingerprint, root_trust_class);

    if !is_dispatchable_trust_class(trust_class) {
        return Err(EngineError::BinUntrusted {
            bin: bin_name,
            fingerprint: fingerprint.unwrap_or_default(),
        });
    }
    let signer_fingerprint = fingerprint.unwrap_or_default();

    Ok(ResolvedBinary {
        absolute_path: bin_path,
        manifest_hash,
        signer_fingerprint,
    })
}

/// Decide whether the trust class returned by
/// [`verify_executor_trust`] is high enough to dispatch the binary.
///
/// Both `TrustedSystem` and `TrustedUser` are dispatchable. The effective
/// tier is already the `min` of the raw binary signature trust and the
/// descriptor's `root_trust_class`, so a `TrustedUser` here means *either*
/// the binary is system-signed and the descriptor capped to user, or the
/// binary itself is user-signed under a user-tier descriptor — both of
/// which are safe to run. Anything weaker (`UntrustedUserSpace`,
/// `Unsigned`) must be refused.
fn is_dispatchable_trust_class(tc: TrustClass) -> bool {
    matches!(tc, TrustClass::TrustedSystem | TrustClass::TrustedUser)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor_resolution::verify_executor_trust;
    use serde_json::json;

    /// Descriptor=System, binary signed by a system-trusted key.
    /// Effective tier = TrustedSystem; gate accepts.
    #[test]
    fn descriptor_system_binary_system_dispatches_as_system() {
        let item_source = json!({
            "signature_info": { "fingerprint": "sys-fp" }
        });
        let (tc, fp) = verify_executor_trust(
            &item_source,
            |f| f == "sys-fp",
            TrustClass::TrustedSystem,
        );
        assert_eq!(tc, TrustClass::TrustedSystem);
        assert_eq!(fp.as_deref(), Some("sys-fp"));
        assert!(is_dispatchable_trust_class(tc));
    }

    /// Descriptor=System, binary signed by a key absent from the trust
    /// store (modeling an untrusted signer under a system root). The
    /// effective tier collapses to UntrustedUserSpace and the gate refuses.
    ///
    /// Note: `verify_executor_trust` does not currently model a raw
    /// `TrustedUser` signer tier — its `raw_trust` is only TrustedSystem,
    /// UntrustedUserSpace, or Unsigned. So a "binary signed by a user-tier
    /// signer" is equivalent to "binary signed by a key not in the trust
    /// store" today; this test covers that.
    #[test]
    fn descriptor_system_unknown_signer_refused() {
        let item_source = json!({
            "signature_info": { "fingerprint": "user-fp" }
        });
        let (tc, fp) = verify_executor_trust(
            &item_source,
            |_| false,
            TrustClass::TrustedSystem,
        );
        assert_eq!(tc, TrustClass::UntrustedUserSpace);
        assert_eq!(fp.as_deref(), Some("user-fp"));
        assert!(!is_dispatchable_trust_class(tc));
    }

    /// Descriptor=User, binary signed by a system-trusted key.
    /// Effective tier = TrustedUser (capped by descriptor); gate accepts.
    ///
    /// This is the case the wave-5 oracle audit flagged: previously the
    /// gate hardcoded acceptance to `TrustedSystem` only, so this case
    /// — which is the *normal* dispatch path for any user-tier descriptor
    /// invoking a system-shipped runtime/handler binary — was rejected.
    #[test]
    fn descriptor_user_binary_system_dispatches_as_user() {
        let item_source = json!({
            "signature_info": { "fingerprint": "sys-fp" }
        });
        let (tc, fp) = verify_executor_trust(
            &item_source,
            |f| f == "sys-fp",
            TrustClass::TrustedUser,
        );
        assert_eq!(tc, TrustClass::TrustedUser);
        assert_eq!(fp.as_deref(), Some("sys-fp"));
        assert!(is_dispatchable_trust_class(tc));
    }

    /// Descriptor=User, binary signed by an unknown signer.
    /// Effective tier = UntrustedUserSpace; gate refuses.
    #[test]
    fn descriptor_user_unknown_signer_refused() {
        let item_source = json!({
            "signature_info": { "fingerprint": "stranger-fp" }
        });
        let (tc, fp) = verify_executor_trust(
            &item_source,
            |_| false,
            TrustClass::TrustedUser,
        );
        assert_eq!(tc, TrustClass::UntrustedUserSpace);
        assert_eq!(fp.as_deref(), Some("stranger-fp"));
        assert!(!is_dispatchable_trust_class(tc));
    }

    /// Sanity floor: anything below TrustedUser must never dispatch.
    #[test]
    fn untrusted_and_unsigned_are_never_dispatchable() {
        assert!(!is_dispatchable_trust_class(TrustClass::UntrustedUserSpace));
        assert!(!is_dispatchable_trust_class(TrustClass::Unsigned));
    }
}
