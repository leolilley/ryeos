//! Native-executor resolution through a cryptographically anchored CAS chain.
//!
//! The mandatory chain is:
//!
//! `trusted signed manifest ref -> exact manifest object -> exact ItemSource -> exact blob`
//!
//! A signer fingerprint stored inside an unsigned CAS object is never evidence
//! of trust. The only trust decision in this module starts from an Ed25519
//! signature over the exact manifest-hash ref body.

use std::collections::HashMap;

use base64::Engine as _;
use lillux::crypto::VerifyingKey;
use serde_json::{Map, Value};

use crate::resolution::TrustClass;

/// Domain separator for the only accepted executor-manifest ref format.
pub const EXECUTOR_MANIFEST_REF_DOMAIN: &str = "ryeos:bundle-executor-manifest";

/// Result of resolving and validating a native executor's ItemSource record.
pub struct ResolvedExecutor {
    /// The exact item reference selected from the verified manifest.
    pub item_ref: String,
    /// The CAS hash of the validated ItemSource object.
    pub item_source_hash: String,
    /// The CAS hash of the content blob (the actual binary bytes).
    pub blob_hash: String,
    /// Unix permission bits (e.g. `0o755`).
    pub mode: u32,
}

impl std::fmt::Debug for ResolvedExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedExecutor")
            .field("item_ref", &self.item_ref)
            .field("item_source_hash", &self.item_source_hash)
            .field("blob_hash", &self.blob_hash)
            .field("mode", &self.mode)
            .finish()
    }
}

/// A trusted, signature-verified pointer to one exact source manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedExecutorManifestRef {
    pub manifest_hash: String,
    pub signer_fingerprint: String,
    pub trust_class: TrustClass,
}

/// Errors that can occur while authenticating or resolving an executor.
#[derive(Debug)]
pub enum ExecutorResolutionError {
    /// The executor ref does not start with `native:` or has an unsafe name.
    NotNativeExecutor,
    /// The binary is not in the manifest for this host triple.
    NotInManifest {
        executor_ref: String,
        host_triple: String,
        available_triples: Vec<String>,
    },
    /// The signed manifest ref is missing fields, malformed, or has an invalid
    /// signature made by a key that is otherwise trusted.
    ManifestRefInvalid { detail: String },
    /// The manifest-ref signer is not trusted by the node.
    ManifestSignerUntrusted { fingerprint: String },
    /// A CAS object's canonical bytes do not match the hash in its parent.
    CasObjectHashMismatch {
        object_kind: &'static str,
        expected: String,
        actual: String,
    },
    /// The source manifest object does not have the exact current schema.
    InvalidManifest { detail: String },
    /// The ItemSource object is missing from CAS.
    ItemSourceMissingFromCas { item_ref: String },
    /// The ItemSource object is malformed or does not match the selected ref.
    InvalidItemSource { item_ref: String, detail: String },
}

impl std::fmt::Display for ExecutorResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotNativeExecutor => write!(f, "executor_ref is not a valid native executor"),
            Self::NotInManifest {
                executor_ref,
                host_triple,
                available_triples,
            } => {
                if available_triples.is_empty() {
                    write!(
                        f,
                        "executor {executor_ref} not found in manifest for host triple {host_triple} \
                         (the bundle ships no binaries for this executor at all — \
                         rebuild with `build-bundle` for {host_triple})"
                    )
                } else {
                    write!(
                        f,
                        "executor {executor_ref} not found in manifest for host triple {host_triple} \
                         (the bundle ships this executor for: {}; rebuild with `build-bundle` \
                         targeting {host_triple} or run on one of the supported hosts)",
                        available_triples.join(", ")
                    )
                }
            }
            Self::ManifestRefInvalid { detail } => {
                write!(f, "bundle executor manifest ref is invalid: {detail}")
            }
            Self::ManifestSignerUntrusted { fingerprint } => write!(
                f,
                "bundle executor manifest signer {fingerprint} is not trusted"
            ),
            Self::CasObjectHashMismatch {
                object_kind,
                expected,
                actual,
            } => write!(
                f,
                "{object_kind} CAS hash mismatch: expected {expected}, got {actual}"
            ),
            Self::InvalidManifest { detail } => {
                write!(f, "bundle executor manifest is invalid: {detail}")
            }
            Self::ItemSourceMissingFromCas { item_ref } => {
                write!(f, "ItemSource for {item_ref} not found in CAS")
            }
            Self::InvalidItemSource { item_ref, detail } => {
                write!(f, "ItemSource for {item_ref} is invalid: {detail}")
            }
        }
    }
}

impl std::error::Error for ExecutorResolutionError {}

/// Verify the mandatory signed manifest-ref format.
///
/// The file must contain exactly one `# ryeos:signed:...` header followed by
/// the executor-manifest domain separator, one lowercase SHA-256 manifest hash, and a trailing
/// newline. The domain-separated body prevents a signature made for another
/// RyeOS artifact from being replayed as an executor-manifest authorization.
/// Plain hash refs are intentionally rejected.
pub fn verify_signed_executor_manifest_ref(
    signed_ref: &str,
    trusted_verifying_key: impl Fn(&str) -> Option<VerifyingKey>,
    root_trust_class: TrustClass,
) -> Result<VerifiedExecutorManifestRef, ExecutorResolutionError> {
    let (signature_line, body) =
        signed_ref
            .split_once('\n')
            .ok_or_else(|| ExecutorResolutionError::ManifestRefInvalid {
                detail: "expected a signature header and manifest-hash body".to_string(),
            })?;
    if !signature_line.starts_with("# ryeos:signed:") || signature_line.trim_end() != signature_line
    {
        return Err(ExecutorResolutionError::ManifestRefInvalid {
            detail: "signature header is not in canonical `# ryeos:signed:` form".to_string(),
        });
    }

    let header =
        lillux::signature::parse_signature_line(signature_line, "#", None).ok_or_else(|| {
            ExecutorResolutionError::ManifestRefInvalid {
                detail: "missing or malformed signature header".to_string(),
            }
        })?;

    let expected_prefix = format!("{EXECUTOR_MANIFEST_REF_DOMAIN}\n");
    let hash_body = body.strip_prefix(&expected_prefix).ok_or_else(|| {
        ExecutorResolutionError::ManifestRefInvalid {
            detail: "missing executor-manifest domain separator".to_string(),
        }
    })?;
    let manifest_hash = hash_body.strip_suffix('\n').ok_or_else(|| {
        ExecutorResolutionError::ManifestRefInvalid {
            detail: "manifest-hash body must end with one newline".to_string(),
        }
    })?;
    if body != format!("{EXECUTOR_MANIFEST_REF_DOMAIN}\n{manifest_hash}\n")
        || !is_lower_sha256(manifest_hash)
    {
        return Err(ExecutorResolutionError::ManifestRefInvalid {
            detail: "body must be exactly one lowercase SHA-256 hash".to_string(),
        });
    }
    if !is_lower_sha256(&header.signer_fingerprint) {
        return Err(ExecutorResolutionError::ManifestRefInvalid {
            detail: "signer fingerprint must be a lowercase SHA-256 value".to_string(),
        });
    }
    if !is_lower_sha256(&header.content_hash)
        || !has_canonical_signature_timestamp_shape(&header.timestamp)
    {
        return Err(ExecutorResolutionError::ManifestRefInvalid {
            detail: "signature hash or timestamp is not canonical".to_string(),
        });
    }
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header.signature_b64)
        .map_err(|_| ExecutorResolutionError::ManifestRefInvalid {
            detail: "signature must use canonical standard base64".to_string(),
        })?;
    if signature_bytes.len() != 64
        || base64::engine::general_purpose::STANDARD.encode(&signature_bytes)
            != header.signature_b64
    {
        return Err(ExecutorResolutionError::ManifestRefInvalid {
            detail: "signature must be one canonical Ed25519 signature".to_string(),
        });
    }

    let verifying_key = trusted_verifying_key(&header.signer_fingerprint).ok_or_else(|| {
        ExecutorResolutionError::ManifestSignerUntrusted {
            fingerprint: header.signer_fingerprint.clone(),
        }
    })?;
    let actual_fingerprint = lillux::signature::compute_fingerprint(&verifying_key);
    if actual_fingerprint != header.signer_fingerprint {
        return Err(ExecutorResolutionError::ManifestRefInvalid {
            detail: "trusted-key lookup returned a key with a different fingerprint".to_string(),
        });
    }
    if !lillux::signature::is_valid_signature_for(
        &header.content_hash,
        &header.signature_b64,
        &header.signer_fingerprint,
        body,
        &verifying_key,
        &actual_fingerprint,
    ) {
        return Err(ExecutorResolutionError::ManifestRefInvalid {
            detail: "content hash or Ed25519 signature verification failed".to_string(),
        });
    }

    Ok(VerifiedExecutorManifestRef {
        manifest_hash: manifest_hash.to_string(),
        signer_fingerprint: header.signer_fingerprint,
        trust_class: TrustClass::TrustedBundle.min(root_trust_class),
    })
}

/// Verify the exact source-manifest CAS object named by the signed ref and
/// return its strictly parsed ItemSource hash map.
pub fn verify_executor_manifest_object(
    manifest_value: &Value,
    expected_hash: &str,
) -> Result<HashMap<String, String>, ExecutorResolutionError> {
    verify_cas_object_hash("source manifest", manifest_value, expected_hash)?;
    let object =
        manifest_value
            .as_object()
            .ok_or_else(|| ExecutorResolutionError::InvalidManifest {
                detail: "top level must be an object".to_string(),
            })?;
    require_exact_keys(object, &["kind", "item_source_hashes"], |detail| {
        ExecutorResolutionError::InvalidManifest { detail }
    })?;
    if object.get("kind").and_then(Value::as_str) != Some("source_manifest") {
        return Err(ExecutorResolutionError::InvalidManifest {
            detail: "kind must be `source_manifest`".to_string(),
        });
    }
    let hashes = object
        .get("item_source_hashes")
        .and_then(Value::as_object)
        .ok_or_else(|| ExecutorResolutionError::InvalidManifest {
            detail: "item_source_hashes must be an object".to_string(),
        })?;

    let mut parsed = HashMap::with_capacity(hashes.len());
    for (item_ref, value) in hashes {
        if item_ref.is_empty() || item_ref.chars().any(char::is_control) {
            return Err(ExecutorResolutionError::InvalidManifest {
                detail: "item_source_hashes contains an invalid item ref".to_string(),
            });
        }
        let hash = value
            .as_str()
            .ok_or_else(|| ExecutorResolutionError::InvalidManifest {
                detail: format!("ItemSource hash for {item_ref} must be a string"),
            })?;
        if !is_lower_sha256(hash) {
            return Err(ExecutorResolutionError::InvalidManifest {
                detail: format!("ItemSource hash for {item_ref} is not lowercase SHA-256"),
            });
        }
        parsed.insert(item_ref.clone(), hash.to_string());
    }
    Ok(parsed)
}

/// Verify one exact ItemSource object selected by the authenticated manifest.
pub fn verify_executor_item_source(
    item_source_value: &Value,
    expected_hash: &str,
    expected_item_ref: &str,
) -> Result<(String, u32), ExecutorResolutionError> {
    verify_cas_object_hash("ItemSource", item_source_value, expected_hash)?;
    let object = item_source_value.as_object().ok_or_else(|| {
        ExecutorResolutionError::InvalidItemSource {
            item_ref: expected_item_ref.to_string(),
            detail: "top level must be an object".to_string(),
        }
    })?;
    require_exact_keys(
        object,
        &[
            "kind",
            "item_ref",
            "content_blob_hash",
            "integrity",
            "signature_info",
            "mode",
        ],
        |detail| ExecutorResolutionError::InvalidItemSource {
            item_ref: expected_item_ref.to_string(),
            detail,
        },
    )?;
    if object.get("kind").and_then(Value::as_str) != Some("item_source") {
        return Err(ExecutorResolutionError::InvalidItemSource {
            item_ref: expected_item_ref.to_string(),
            detail: "kind must be `item_source`".to_string(),
        });
    }
    let actual_item_ref = object.get("item_ref").and_then(Value::as_str).unwrap_or("");
    if actual_item_ref != expected_item_ref {
        return Err(ExecutorResolutionError::InvalidItemSource {
            item_ref: expected_item_ref.to_string(),
            detail: format!("record names `{actual_item_ref}`"),
        });
    }
    let blob_hash = object
        .get("content_blob_hash")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !is_lower_sha256(blob_hash) {
        return Err(ExecutorResolutionError::InvalidItemSource {
            item_ref: expected_item_ref.to_string(),
            detail: "content_blob_hash must be lowercase SHA-256".to_string(),
        });
    }
    let expected_integrity = format!("sha256:{blob_hash}");
    if object.get("integrity").and_then(Value::as_str) != Some(expected_integrity.as_str()) {
        return Err(ExecutorResolutionError::InvalidItemSource {
            item_ref: expected_item_ref.to_string(),
            detail: "integrity must exactly match content_blob_hash".to_string(),
        });
    }
    if object.get("signature_info") != Some(&Value::Null) {
        return Err(ExecutorResolutionError::InvalidItemSource {
            item_ref: expected_item_ref.to_string(),
            detail: "signature_info must be null; trust comes from the signed manifest".to_string(),
        });
    }
    let mode_u64 = object.get("mode").and_then(Value::as_u64).ok_or_else(|| {
        ExecutorResolutionError::InvalidItemSource {
            item_ref: expected_item_ref.to_string(),
            detail: "mode must be an integer".to_string(),
        }
    })?;
    if mode_u64 > 0o777 || mode_u64 & 0o111 == 0 {
        return Err(ExecutorResolutionError::InvalidItemSource {
            item_ref: expected_item_ref.to_string(),
            detail: "mode must contain an execute bit and no special permission bits".to_string(),
        });
    }

    Ok((blob_hash.to_string(), mode_u64 as u32))
}

/// Resolve a native executor from an already authenticated manifest map.
///
/// The selected ItemSource object is hash-checked and strictly validated here.
/// Blob existence and raw-byte hashing remain the materializer's responsibility.
pub fn resolve_native_executor(
    manifest_item_source_hashes: &HashMap<String, String>,
    executor_ref: &str,
    host_triple: &str,
    cas_get_object: impl Fn(&str) -> Result<Option<Value>, String>,
) -> Result<ResolvedExecutor, ExecutorResolutionError> {
    let bare = executor_ref
        .strip_prefix("native:")
        .ok_or(ExecutorResolutionError::NotNativeExecutor)?;
    if !is_safe_native_name(bare) {
        return Err(ExecutorResolutionError::NotNativeExecutor);
    }

    let item_ref = format!("bin/{host_triple}/{bare}");
    let object_hash = manifest_item_source_hashes.get(&item_ref).ok_or_else(|| {
        let suffix = format!("/{bare}");
        let mut available_triples: Vec<String> = manifest_item_source_hashes
            .keys()
            .filter_map(|key| {
                key.strip_prefix("bin/")
                    .and_then(|rest| rest.strip_suffix(&suffix))
            })
            .map(str::to_string)
            .collect();
        available_triples.sort();
        available_triples.dedup();
        ExecutorResolutionError::NotInManifest {
            executor_ref: executor_ref.to_string(),
            host_triple: host_triple.to_string(),
            available_triples,
        }
    })?;

    let item_source_value = cas_get_object(object_hash)
        .map_err(|_| ExecutorResolutionError::ItemSourceMissingFromCas {
            item_ref: item_ref.clone(),
        })?
        .ok_or_else(|| ExecutorResolutionError::ItemSourceMissingFromCas {
            item_ref: item_ref.clone(),
        })?;
    let (blob_hash, mode) =
        verify_executor_item_source(&item_source_value, object_hash, &item_ref)?;

    Ok(ResolvedExecutor {
        item_ref,
        item_source_hash: object_hash.clone(),
        blob_hash,
        mode,
    })
}

fn verify_cas_object_hash(
    object_kind: &'static str,
    value: &Value,
    expected_hash: &str,
) -> Result<(), ExecutorResolutionError> {
    if !is_lower_sha256(expected_hash) {
        return Err(ExecutorResolutionError::CasObjectHashMismatch {
            object_kind,
            expected: expected_hash.to_string(),
            actual: "<invalid expected hash>".to_string(),
        });
    }
    let actual = lillux::cas::canonical_json(value)
        .map(|canonical| lillux::cas::sha256_hex(canonical.as_bytes()))
        .map_err(|_| ExecutorResolutionError::CasObjectHashMismatch {
            object_kind,
            expected: expected_hash.to_string(),
            actual: "<uncanonicalizable object>".to_string(),
        })?;
    if actual != expected_hash {
        return Err(ExecutorResolutionError::CasObjectHashMismatch {
            object_kind,
            expected: expected_hash.to_string(),
            actual,
        });
    }
    Ok(())
}

fn require_exact_keys<E>(
    object: &Map<String, Value>,
    expected: &[&str],
    make_error: impl FnOnce(String) -> E,
) -> Result<(), E> {
    let mut actual: Vec<&str> = object.keys().map(String::as_str).collect();
    actual.sort_unstable();
    let mut expected_sorted = expected.to_vec();
    expected_sorted.sort_unstable();
    if actual != expected_sorted {
        return Err(make_error(format!(
            "expected fields [{}], got [{}]",
            expected_sorted.join(", "),
            actual.join(", ")
        )));
    }
    Ok(())
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Validate the fixed-width UTC timestamp shape emitted by
/// `lillux::time::iso8601_now`.
///
/// Signature timestamps are informational metadata: the Ed25519 proof covers
/// the content hash, not this header field. This check only prevents alternate
/// timestamp encodings from entering the canonical executor artifact format.
pub(crate) fn has_canonical_signature_timestamp_shape(timestamp: &str) -> bool {
    let bytes = timestamp.as_bytes();
    bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        })
}

fn is_safe_native_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_whitespace)
        && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use serde_json::json;

    fn sign_manifest_ref(hash: &str, key: &SigningKey) -> String {
        lillux::signature::sign_content(
            &format!("{EXECUTOR_MANIFEST_REF_DOMAIN}\n{hash}\n"),
            key,
            "#",
            None,
        )
    }

    fn trusted_key<'a>(key: &'a SigningKey) -> impl Fn(&str) -> Option<VerifyingKey> + 'a {
        let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
        move |candidate| (candidate == fingerprint.as_str()).then(|| key.verifying_key())
    }

    #[test]
    fn signed_manifest_ref_verifies_and_is_capped_by_root() {
        let key = SigningKey::from_bytes(&[19; 32]);
        let hash = "ab".repeat(32);
        let verified = verify_signed_executor_manifest_ref(
            &sign_manifest_ref(&hash, &key),
            trusted_key(&key),
            TrustClass::TrustedProject,
        )
        .unwrap();

        assert_eq!(verified.manifest_hash, hash);
        assert_eq!(verified.trust_class, TrustClass::TrustedProject);
    }

    #[test]
    fn plain_or_tampered_manifest_ref_is_rejected() {
        let key = SigningKey::from_bytes(&[20; 32]);
        let hash = "ab".repeat(32);
        assert!(verify_signed_executor_manifest_ref(
            &format!("{hash}\n"),
            trusted_key(&key),
            TrustClass::TrustedBundle,
        )
        .is_err());

        let cross_protocol_signature =
            lillux::signature::sign_content(&format!("{hash}\n"), &key, "#", None);
        assert!(verify_signed_executor_manifest_ref(
            &cross_protocol_signature,
            trusted_key(&key),
            TrustClass::TrustedBundle,
        )
        .is_err());

        let signed = sign_manifest_ref(&hash, &key).replace(&hash, &"cd".repeat(32));
        assert!(verify_signed_executor_manifest_ref(
            &signed,
            trusted_key(&key),
            TrustClass::TrustedBundle,
        )
        .is_err());
    }

    #[test]
    fn signed_manifest_ref_requires_fixed_width_utc_timestamp() {
        let key = SigningKey::from_bytes(&[21; 32]);
        let hash = "ab".repeat(32);
        let signed = sign_manifest_ref(&hash, &key);
        let (signature_line, body) = signed.split_once('\n').unwrap();
        let header = lillux::signature::parse_signature_line(signature_line, "#", None).unwrap();
        let noncanonical = format!(
            "# ryeos:signed:2026-07-14T12:34:56+00:00:{}:{}:{}\n{body}",
            header.content_hash, header.signature_b64, header.signer_fingerprint
        );

        assert!(matches!(
            verify_signed_executor_manifest_ref(
                &noncanonical,
                trusted_key(&key),
                TrustClass::TrustedBundle,
            ),
            Err(ExecutorResolutionError::ManifestRefInvalid { ref detail })
                if detail.contains("timestamp")
        ));
    }

    #[test]
    fn canonical_signature_timestamp_shape_matches_emitted_form() {
        assert!(has_canonical_signature_timestamp_shape(
            "2026-07-14T12:34:56Z"
        ));
        assert!(!has_canonical_signature_timestamp_shape(
            "2026-07-14T12:34:56+00:00"
        ));
        assert!(!has_canonical_signature_timestamp_shape(
            "2026-07-14T12:34:56.000Z"
        ));
        assert!(!has_canonical_signature_timestamp_shape(
            "2026-7-14T12:34:56Z"
        ));
        assert!(!has_canonical_signature_timestamp_shape(
            "2026-07-14 12:34:56Z"
        ));
    }

    #[test]
    fn item_source_requires_exact_current_shape() {
        let item_ref = "bin/x86_64-unknown-linux-gnu/demo";
        let blob_hash = "ab".repeat(32);
        let mut value = json!({
            "kind": "item_source",
            "item_ref": item_ref,
            "content_blob_hash": blob_hash,
            "integrity": format!("sha256:{blob_hash}"),
            "signature_info": null,
            "mode": 0o755,
        });
        let hash = lillux::cas::sha256_hex(lillux::cas::canonical_json(&value).unwrap().as_bytes());
        assert!(verify_executor_item_source(&value, &hash, item_ref).is_ok());

        value["signature_info"] = json!({"fingerprint": "claimed-only"});
        let extra_field_hash =
            lillux::cas::sha256_hex(lillux::cas::canonical_json(&value).unwrap().as_bytes());
        assert!(verify_executor_item_source(&value, &extra_field_hash, item_ref).is_err());
    }

    #[test]
    fn manifest_object_must_match_signed_hash_exactly() {
        let mut value = json!({
            "kind": "source_manifest",
            "item_source_hashes": {
                "bin/x86_64-unknown-linux-gnu/demo": "ab".repeat(32),
            },
        });
        let hash = lillux::cas::sha256_hex(lillux::cas::canonical_json(&value).unwrap().as_bytes());
        assert!(verify_executor_manifest_object(&value, &hash).is_ok());

        value["item_source_hashes"]["bin/x86_64-unknown-linux-gnu/demo"] =
            Value::String("cd".repeat(32));
        assert!(matches!(
            verify_executor_manifest_object(&value, &hash),
            Err(ExecutorResolutionError::CasObjectHashMismatch {
                object_kind: "source manifest",
                ..
            })
        ));
    }
}
