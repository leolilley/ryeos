//! Executor resolution from the verified CAS chain.
//!
//! When a tool YAML declares `executor_ref: native:<bin>`, the engine
//! resolves it by looking up the binary's `item_source` record in the
//! manifest. The binary's integrity is anchored by a signed `item_source`
//! JSON object stored in CAS.

use std::collections::HashMap;
use serde_json::Value;

use crate::resolution::TrustClass;

/// Result of resolving a native executor from the CAS manifest.
pub struct ResolvedExecutor {
    /// The binary's `ItemSource` JSON value from CAS.
    pub item_source: Value,
    /// The CAS hash of the content blob (the actual binary bytes).
    pub blob_hash: String,
    /// Unix permission bits (e.g. `0o755`).
    pub mode: u32,
}

impl std::fmt::Debug for ResolvedExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedExecutor")
            .field("blob_hash", &self.blob_hash)
            .field("mode", &self.mode)
            .finish()
    }
}

/// Errors that can occur during executor resolution.
#[derive(Debug)]
pub enum ExecutorResolutionError {
    /// The executor ref does not start with `native:`.
    NotNativeExecutor,
    /// The binary is not in the manifest for this host triple.
    NotInManifest {
        executor_ref: String,
        host_triple: String,
    },
    /// The item_source object is missing from CAS.
    ItemSourceMissingFromCas {
        item_ref: String,
    },
    /// The item_source record has no mode (binary must have exec bit).
    MissingMode {
        item_ref: String,
    },
}

impl std::fmt::Display for ExecutorResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotNativeExecutor => write!(f, "executor_ref is not a native executor"),
            Self::NotInManifest { executor_ref, host_triple } => {
                write!(f, "executor {executor_ref} not found in manifest for host triple {host_triple}")
            }
            Self::ItemSourceMissingFromCas { item_ref } => {
                write!(f, "item_source for {item_ref} not found in CAS")
            }
            Self::MissingMode { item_ref } => {
                write!(f, "item_source for {item_ref} has no mode field (binary must have exec bit)")
            }
        }
    }
}

impl std::error::Error for ExecutorResolutionError {}

/// Resolve a native executor from the CAS manifest.
///
/// Looks up `bin/<host_triple>/<bare>` in the manifest's
/// `item_source_hashes`, fetches the `ItemSource` from CAS, and
/// returns a `ResolvedExecutor` ready for trust verification.
///
/// **Path matching rule:** no fallback. If `bin/<host_triple>/<bare>`
/// is not in the manifest, resolution fails.
pub fn resolve_native_executor(
    manifest_item_source_hashes: &HashMap<String, String>,
    executor_ref: &str,
    host_triple: &str,
    cas_get_object: impl Fn(&str) -> std::result::Result<Option<Value>, String>,
) -> std::result::Result<ResolvedExecutor, ExecutorResolutionError> {
    let bare = executor_ref
        .strip_prefix("native:")
        .ok_or(ExecutorResolutionError::NotNativeExecutor)?
        .trim();

    if bare.is_empty() {
        return Err(ExecutorResolutionError::NotNativeExecutor);
    }

    let item_ref = format!("bin/{host_triple}/{bare}");

    let object_hash = manifest_item_source_hashes
        .get(&item_ref)
        .ok_or(ExecutorResolutionError::NotInManifest {
            executor_ref: executor_ref.to_string(),
            host_triple: host_triple.to_string(),
        })?;

    let item_source_value = cas_get_object(object_hash)
        .map_err(|_| ExecutorResolutionError::ItemSourceMissingFromCas {
            item_ref: item_ref.clone(),
        })?
        .ok_or(ExecutorResolutionError::ItemSourceMissingFromCas {
            item_ref: item_ref.clone(),
        })?;

    let blob_hash = item_source_value
        .get("content_blob_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mode = item_source_value
        .get("mode")
        .and_then(|v| v.as_u64())
        .map(|m| m as u32)
        .ok_or(ExecutorResolutionError::MissingMode {
            item_ref: item_ref.clone(),
        })?;

    Ok(ResolvedExecutor {
        item_source: item_source_value,
        blob_hash,
        mode,
    })
}

/// A verified executor ready for materialization.
pub struct VerifiedExecutor {
    pub blob_hash: String,
    pub mode: u32,
    pub trust_class: TrustClass,
}

/// Verify the trust status of a binary's `item_source` record.
///
/// The item_source JSON contains `signature_info.fingerprint` and
/// `signature_info.signature`. The fingerprint is checked against
/// the trust store. If the signer is trusted, the binary is trusted.
/// If no signature_info is present, the binary is unsigned.
pub fn verify_executor_trust(
    item_source_value: &Value,
    trust_store_has_fingerprint: impl Fn(&str) -> bool,
) -> (TrustClass, Option<String>) {
    if let Some(sig_info) = item_source_value.get("signature_info") {
        let fingerprint = sig_info
            .get("fingerprint")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if fingerprint.is_empty() {
            return (TrustClass::UntrustedUserSpace, None);
        }

        if trust_store_has_fingerprint(fingerprint) {
            (TrustClass::TrustedSystem, Some(fingerprint.to_string()))
        } else {
            (TrustClass::UntrustedUserSpace, Some(fingerprint.to_string()))
        }
    } else {
        (TrustClass::Unsigned, None)
    }
}
