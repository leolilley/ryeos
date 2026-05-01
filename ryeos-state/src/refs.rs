//! Signed ref read/write/verify.
//!
//! One authoritative signed mutable pointer per chain:
//! `.ai/state/objects/refs/generic/chains/<chain_root_id>/head`
//!
//! Signed ref format:
//! ```json
//! {
//!   "schema": 1,
//!   "kind": "signed_ref",
//!   "ref_path": "chains/T-root/head",
//!   "target_hash": "<chain_state_hash>",
//!   "updated_at": "...",
//!   "signer": "<node-fingerprint>",
//!   "signature": "<ed25519-sig-over-canonical-json-without-signature-field>"
//! }
//! ```

use anyhow::{anyhow, Context};
use base64::Engine as _;
use lillux::crypto::Verifier;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

use crate::signer::Signer;

const SIGNED_REF_SCHEMA: u32 = 1;
const SIGNED_REF_KIND: &str = "signed_ref";

/// A signed reference — an authoritative mutable pointer to a CAS object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRef {
    pub schema: u32,
    pub kind: String,
    pub ref_path: String,
    pub target_hash: String,
    pub updated_at: String,
    pub signer: String,
    /// Signature is computed over the object WITHOUT this field.
    pub signature: String,
}

impl SignedRef {
    /// Create a new signed ref (without signature).
    pub fn new(
        ref_path: String,
        target_hash: String,
        updated_at: String,
        signer: String,
    ) -> Self {
        Self {
            schema: SIGNED_REF_SCHEMA,
            kind: SIGNED_REF_KIND.to_string(),
            ref_path,
            target_hash,
            updated_at,
            signer,
            signature: String::new(),
        }
    }

    /// Validate the ref object structure (not signature).
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != SIGNED_REF_SCHEMA {
            anyhow::bail!(
                "invalid schema: expected {}, got {}",
                SIGNED_REF_SCHEMA,
                self.schema
            );
        }
        if self.kind != SIGNED_REF_KIND {
            anyhow::bail!(
                "invalid kind: expected {}, got {}",
                SIGNED_REF_KIND,
                self.kind
            );
        }
        if self.ref_path.is_empty() {
            anyhow::bail!("ref_path must not be empty");
        }
        if !lillux::valid_hash(&self.target_hash) {
            anyhow::bail!("invalid target_hash: {}", self.target_hash);
        }
        if self.updated_at.is_empty() {
            anyhow::bail!("updated_at must not be empty");
        }
        if self.signer.is_empty() {
            anyhow::bail!("signer must not be empty");
        }
        if self.signature.is_empty() {
            anyhow::bail!("signature must not be empty");
        }
        Ok(())
    }

    /// Return a copy of this ref without the signature field (for signing/verifying).
    fn without_signature(&self) -> Value {
        json!({
            "schema": self.schema,
            "kind": self.kind,
            "ref_path": self.ref_path,
            "target_hash": self.target_hash,
            "updated_at": self.updated_at,
            "signer": self.signer,
        })
    }

    /// Convert to serde_json::Value.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Write a signed ref atomically to a file.
///
/// The signature is computed over the canonical JSON representation
/// of the ref WITHOUT the signature field.
pub fn write_signed_ref(
    path: &Path,
    mut signed_ref: SignedRef,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    // Compute signature over the ref without the signature field
    let unsigned = signed_ref.without_signature();
    let canonical = lillux::canonical_json(&unsigned);
    let sig_bytes = signer.sign(canonical.as_bytes());
    signed_ref.signature = base64::engine::general_purpose::STANDARD.encode(sig_bytes);

    // Validate the ref
    signed_ref.validate()?;

    // Serialize to canonical JSON
    let value = signed_ref.to_value();
    let canonical = lillux::canonical_json(&value);

    // Create parent directories
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create parent directories")?;
    }

    // Atomic write
    lillux::atomic_write(path, canonical.as_bytes()).context("failed to write signed ref")?;

    Ok(())
}

/// Read a signed ref and verify its signature against a trust store.
///
/// This is the safe variant of [`read_signed_ref`] that also validates
/// the cryptographic signature. Use this on all root-discovery paths
/// where untrusted data could be tampered with.
pub fn read_verified_ref(
    path: &Path,
    trust_store: &TrustStore,
) -> anyhow::Result<SignedRef> {
    let signed_ref = read_signed_ref(path)?;
    verify_signed_ref(&signed_ref, trust_store)?;
    Ok(signed_ref)
}

/// Read a signed ref from a file.
pub fn read_signed_ref(path: &Path) -> anyhow::Result<SignedRef> {
    let content = fs::read_to_string(path).context("failed to read signed ref")?;
    let value: Value = serde_json::from_str(&content).context("failed to parse signed ref JSON")?;
    let signed_ref: SignedRef =
        serde_json::from_value(value).context("failed to deserialize signed ref")?;
    signed_ref.validate()?;
    Ok(signed_ref)
}

/// Verify a signed ref's signature against a trust store.
///
/// The signature must be valid over the canonical JSON representation
/// of the ref WITHOUT the signature field, signed by the signer's key.
pub fn verify_signed_ref(signed_ref: &SignedRef, verifying_keys: &TrustStore) -> anyhow::Result<()> {
    signed_ref.validate()?;

    // Look up the signer's public key in the trust store
    let pubkey = verifying_keys
        .get(&signed_ref.signer)
        .ok_or_else(|| anyhow!("signer {} not in trust store", signed_ref.signer))?;

    // Reconstruct the canonical JSON without the signature
    let unsigned = signed_ref.without_signature();
    let canonical = lillux::canonical_json(&unsigned);

    // Decode the signature from base64
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&signed_ref.signature)
        .context("failed to decode signature")?;

    // Convert to Signature
    let signature = lillux::crypto::Signature::from_slice(&sig_bytes)
        .map_err(|e| anyhow!("failed to parse signature: {}", e))?;

    // Verify
    pubkey
        .verify(canonical.as_bytes(), &signature)
        .map_err(|e| anyhow!("signature verification failed: {}", e))?;

    Ok(())
}

/// Trust store — map of fingerprint → public key.
pub type TrustStore = std::collections::HashMap<String, lillux::crypto::VerifyingKey>;

/// Write a project head ref. The project_hash should be derived from the project path
/// (similar to how chain_root_id identifies chains).
pub fn write_project_head_ref(
    refs_root: &Path,
    project_hash: &str,
    project_snapshot_hash: &str,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let ref_path = format!("projects/{}", project_hash);
    let signed_ref = SignedRef::new(
        ref_path.clone(),
        project_snapshot_hash.to_string(),
        lillux::time::iso8601_now(),
        signer.fingerprint().to_string(),
    );
    let path = refs_root.join(&ref_path).join("head");
    write_signed_ref(&path, signed_ref, signer)
}

/// Read a project head ref. Returns the target hash (project snapshot hash).
pub fn read_project_head_ref(
    refs_root: &Path,
    project_hash: &str,
) -> anyhow::Result<Option<String>> {
    let head_path = refs_root.join(format!("projects/{}", project_hash)).join("head");
    if !head_path.exists() {
        return Ok(None);
    }
    let signed_ref = read_signed_ref(&head_path)?;
    Ok(Some(signed_ref.target_hash))
}

/// Advance a project head ref (CAS-first, with conflict detection).
/// The `current_hash` must match the current head, or it fails.
/// This is the project equivalent of advancing a chain head.
pub fn advance_project_head_ref(
    refs_root: &Path,
    project_hash: &str,
    new_snapshot_hash: &str,
    signer: &dyn Signer,
) -> anyhow::Result<()> {
    let current = read_project_head_ref(refs_root, project_hash)?
        .ok_or_else(|| anyhow!("no project head ref for project {}", project_hash))?;

    if current != new_snapshot_hash {
        anyhow::bail!(
            "project head conflict for project {}: expected {}, got {}",
            project_hash, current, new_snapshot_hash
        );
    }

    write_project_head_ref(refs_root, project_hash, new_snapshot_hash, signer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::TestSigner;

    fn make_signed_ref() -> SignedRef {
        SignedRef::new(
            "chains/T-root/head".to_string(),
            "01".repeat(32),
            "2026-04-21T12:00:00Z".to_string(),
            "abcd1234".to_string(),
        )
    }

    #[test]
    fn signed_ref_validation_passes() {
        let mut r = make_signed_ref();
        r.signature = "valid_sig".to_string();
        assert!(r.validate().is_ok());
    }

    #[test]
    fn signed_ref_validation_rejects_bad_schema() {
        let mut r = make_signed_ref();
        r.schema = 999;
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_bad_kind() {
        let mut r = make_signed_ref();
        r.kind = "wrong_kind".to_string();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_empty_ref_path() {
        let mut r = make_signed_ref();
        r.ref_path = String::new();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_invalid_target_hash() {
        let mut r = make_signed_ref();
        r.target_hash = "not_a_hash".to_string();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_empty_updated_at() {
        let mut r = make_signed_ref();
        r.updated_at = String::new();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_empty_signer() {
        let mut r = make_signed_ref();
        r.signer = String::new();
        r.signature = "sig".to_string();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_validation_rejects_empty_signature() {
        let r = make_signed_ref();
        assert!(r.validate().is_err());
    }

    #[test]
    fn signed_ref_serialization_roundtrip() {
        let mut r = make_signed_ref();
        r.signature = "test_sig".to_string();
        let json = serde_json::to_string(&r).unwrap();
        let r2: SignedRef = serde_json::from_str(&json).unwrap();
        assert_eq!(r.ref_path, r2.ref_path);
        assert_eq!(r.target_hash, r2.target_hash);
        assert_eq!(r.signature, r2.signature);
    }

    #[test]
    fn signed_ref_to_value_is_valid_json() {
        let mut r = make_signed_ref();
        r.signature = "sig".to_string();
        let value = r.to_value();
        assert!(value.is_object());
        assert_eq!(value["schema"], 1);
        assert_eq!(value["kind"], "signed_ref");
    }

    #[test]
    fn signed_ref_without_signature_excludes_signature() {
        let mut r = make_signed_ref();
        r.signature = "should_be_excluded".to_string();
        let unsigned = r.without_signature();
        assert!(!unsigned.as_object().unwrap().contains_key("signature"));
    }

    #[test]
    fn write_and_read_signed_ref() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("test_ref");
        let signer = TestSigner::default();

        let signed_ref = SignedRef::new(
            "chains/T-test/head".to_string(),
            "02".repeat(32),
            "2026-04-21T13:00:00Z".to_string(),
            signer.fingerprint().to_string(),
        );

        write_signed_ref(&path, signed_ref.clone(), &signer).unwrap();
        assert!(path.exists());

        let read_ref = read_signed_ref(&path).unwrap();
        assert_eq!(read_ref.ref_path, signed_ref.ref_path);
        assert_eq!(read_ref.target_hash, signed_ref.target_hash);
        assert!(!read_ref.signature.is_empty());
    }

    #[test]
    fn write_signed_ref_creates_parent_dirs() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("deep/nested/dir/ref");
        let signer = TestSigner::default();

        let signed_ref = SignedRef::new(
            "chains/T-test/head".to_string(),
            "03".repeat(32),
            "2026-04-21T14:00:00Z".to_string(),
            signer.fingerprint().to_string(),
        );

        write_signed_ref(&path, signed_ref, &signer).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn signed_ref_canonical_json_determinism() {
        let mut r1 = make_signed_ref();
        r1.signature = "same_sig".to_string();

        let mut r2 = make_signed_ref();
        r2.signature = "same_sig".to_string();

        let json1 = lillux::canonical_json(&r1.to_value());
        let json2 = lillux::canonical_json(&r2.to_value());
        assert_eq!(json1, json2);
    }
}
