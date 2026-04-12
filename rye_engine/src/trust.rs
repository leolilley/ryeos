//! Item signer trust store and signature verification.
//!
//! Loads trusted signer public keys from `{state_dir}/trust/trusted_keys/`.
//! Verifies Ed25519 item signatures and computes content hashes over
//! the post-signature-line content.
//!
//! The trust store is a simple key-value map: signer fingerprint → public key.
//! It does NOT share trust policy with daemon request auth or node auth —
//! those are distinct trust domains (see 05-trust-auth-and-signatures.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::contracts::{
    ResolvedItem, SignatureEnvelope, SignatureHeader, SignerFingerprint, TrustClass,
    VerifiedItem,
};
use crate::error::EngineError;

// ── Trusted signer entry ────────────────────────────────────────────

/// A trusted signer loaded from the trust store.
#[derive(Debug, Clone)]
pub struct TrustedSigner {
    pub fingerprint: String,
    pub verifying_key: VerifyingKey,
    pub label: Option<String>,
}

// ── Trust store ─────────────────────────────────────────────────────

/// Item signer trust store.
///
/// Maps signer fingerprints to their Ed25519 verifying keys.
/// Loaded from `*.pub` or `*.toml` key files in the trusted keys directory.
///
/// This is the **item signer** trust domain only. Distinct from
/// principal auth and node trust.
#[derive(Debug, Clone)]
pub struct TrustStore {
    signers: HashMap<String, TrustedSigner>,
}

impl TrustStore {
    /// Create an empty trust store (no signers trusted).
    pub fn empty() -> Self {
        Self {
            signers: HashMap::new(),
        }
    }

    /// Create a trust store from a pre-built map of signers.
    pub fn from_signers(signers: Vec<TrustedSigner>) -> Self {
        let map = signers
            .into_iter()
            .map(|s| (s.fingerprint.clone(), s))
            .collect();
        Self { signers: map }
    }

    /// Load trusted signer keys from a directory.
    ///
    /// Each file in `keys_dir` should contain a public key in one of:
    ///   - Raw base64-encoded 32-byte Ed25519 public key (`.pub` files)
    ///   - `ed25519:<base64>` format (same as identity doc)
    ///
    /// The fingerprint is the SHA-256 hex digest of the raw public key bytes.
    pub fn load_from_dir(keys_dir: &Path) -> Result<Self, EngineError> {
        if !keys_dir.exists() {
            return Ok(Self::empty());
        }

        let entries = std::fs::read_dir(keys_dir).map_err(|e| EngineError::Internal(
            format!("cannot read trust store dir {}: {e}", keys_dir.display()),
        ))?;

        let mut signers = HashMap::new();

        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        paths.sort();

        for path in &paths {
            let has_key_ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "pub" || e == "key")
                .unwrap_or(false);

            match load_signer_key(&path) {
                Ok(signer) => {
                    tracing::debug!(fingerprint = %signer.fingerprint, path = %path.display(), "loaded trusted signer key");
                    signers.insert(signer.fingerprint.clone(), signer);
                }
                Err(e) => {
                    if has_key_ext {
                        // Key files that fail to parse are hard errors
                        return Err(e);
                    }
                    // Non-key files (readme.txt, etc.) are silently skipped
                    continue;
                }
            }
        }

        Ok(Self { signers })
    }

    /// Check whether a fingerprint is trusted.
    pub fn is_trusted(&self, fingerprint: &str) -> bool {
        self.signers.contains_key(fingerprint)
    }

    /// Look up a signer's verifying key.
    pub fn get(&self, fingerprint: &str) -> Option<&TrustedSigner> {
        self.signers.get(fingerprint)
    }

    /// Number of trusted signers.
    pub fn len(&self) -> usize {
        self.signers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.signers.is_empty()
    }
}

// ── Key loading ─────────────────────────────────────────────────────

/// Load a single signer public key from a file.
///
/// Supports two formats:
///   - `ed25519:<base64>` (one line)
///   - Raw base64-encoded 32-byte key (one line)
fn load_signer_key(path: &Path) -> Result<TrustedSigner, EngineError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        EngineError::Internal(format!("cannot read key file {}: {e}", path.display()))
    })?;

    let line = raw.lines().next().unwrap_or("").trim();

    let key_b64 = if let Some(stripped) = line.strip_prefix("ed25519:") {
        stripped
    } else {
        line
    };

    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(key_b64)
        .map_err(|e| {
            EngineError::Internal(format!(
                "invalid base64 in key file {}: {e}",
                path.display()
            ))
        })?;

    let key_array: [u8; 32] = key_bytes.try_into().map_err(|_| {
        EngineError::Internal(format!(
            "key file {} must contain exactly 32 bytes, got {}",
            path.display(),
            key_b64.len()
        ))
    })?;

    let verifying_key = VerifyingKey::from_bytes(&key_array).map_err(|e| {
        EngineError::Internal(format!(
            "invalid Ed25519 public key in {}: {e}",
            path.display()
        ))
    })?;

    let fingerprint = compute_fingerprint(&verifying_key);

    // Use filename stem as label
    let label = path.file_stem().and_then(|s| s.to_str()).map(String::from);

    Ok(TrustedSigner {
        fingerprint,
        verifying_key,
        label,
    })
}

// ── Fingerprint computation ─────────────────────────────────────────

/// Compute the SHA-256 hex fingerprint of an Ed25519 public key.
///
/// This matches the fingerprint computation in `ryeosd/src/identity.rs`.
pub fn compute_fingerprint(key: &VerifyingKey) -> String {
    let hash = Sha256::digest(key.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hash.iter() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

// ── Content hash after signature line ───────────────────────────────

/// Compute the content hash over file bytes AFTER the signature line.
///
/// The signature line itself is excluded from the hash. This is what
/// the signer hashed when creating the signature.
///
/// Returns the SHA-256 hex digest of the post-signature content, and
/// the byte offset where the content starts (after the signature line's
/// trailing newline).
pub fn content_hash_after_signature(
    content: &str,
    envelope: &SignatureEnvelope,
) -> Option<String> {
    let sig_line_end = find_signature_line_end(content, envelope)?;
    let after = &content[sig_line_end..];
    Some(sha256_hex(after.as_bytes()))
}

/// Find the byte offset immediately after the signature line (including
/// its trailing newline if present).
///
/// Respects `after_shebang`: if true, looks at line 2 first, then line 1.
fn find_signature_line_end(content: &str, envelope: &SignatureEnvelope) -> Option<usize> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let candidates: Vec<usize> = if envelope.after_shebang {
        let mut c = Vec::new();
        if lines.len() > 1 {
            c.push(1);
        }
        c.push(0);
        c
    } else {
        vec![0]
    };

    for idx in candidates {
        if is_signature_line(lines[idx], envelope) {
            // Compute byte offset: sum of all lines up to and including this one
            // plus their newline bytes
            let mut offset = 0;
            for i in 0..=idx {
                offset += lines[i].len();
                // Check if there's a newline after this line
                let pos = offset;
                if pos < content.len() {
                    let byte = content.as_bytes()[pos];
                    if byte == b'\n' {
                        offset += 1;
                    } else if byte == b'\r' {
                        offset += 1;
                        if offset < content.len() && content.as_bytes()[offset] == b'\n' {
                            offset += 1;
                        }
                    }
                }
            }
            return Some(offset);
        }
    }

    None
}

/// Check whether a line is a signature line for the given envelope.
fn is_signature_line(line: &str, envelope: &SignatureEnvelope) -> bool {
    let trimmed = line.trim();
    let after_prefix = match trimmed.strip_prefix(envelope.prefix.as_str()) {
        Some(s) => s.trim_start(),
        None => return false,
    };

    let payload_area = if let Some(ref suffix) = envelope.suffix {
        match after_prefix.trim_end().strip_suffix(suffix.as_str()) {
            Some(s) => s.trim_end(),
            None => return false,
        }
    } else {
        after_prefix.trim_end()
    };

    payload_area.starts_with("rye:signed:")
}

// ── Signature verification ──────────────────────────────────────────

/// Verify an item's Ed25519 signature.
///
/// Steps:
/// 1. Recompute content hash over bytes after the signature line
/// 2. Compare with the content_hash in the signature header
/// 3. Verify the Ed25519 signature over the content_hash string
///
/// The signature is over the content_hash hex string (as bytes), matching
/// the pattern in `ryeosd/src/auth.rs` and `ryeosd/src/identity.rs`.
pub fn verify_item_signature(
    content: &str,
    header: &SignatureHeader,
    envelope: &SignatureEnvelope,
    trust_store: &TrustStore,
) -> Result<(TrustClass, Option<SignerFingerprint>), EngineError> {
    // Step 1: Recompute content hash over post-signature content
    let actual_hash = content_hash_after_signature(content, envelope).ok_or_else(|| {
        EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(), // caller fills this in
            reason: "could not locate signature line in content".into(),
        }
    })?;

    // Step 2: Compare content hashes
    if actual_hash != header.content_hash {
        return Err(EngineError::ContentHashMismatch {
            canonical_ref: String::new(),
            expected: header.content_hash.clone(),
            actual: actual_hash,
        });
    }

    // Step 3: Decode and verify the Ed25519 signature
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header.signature_b64)
        .map_err(|e| EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(),
            reason: format!("invalid base64 in signature: {e}"),
        })?;

    let signature = Signature::from_slice(&sig_bytes).map_err(|e| {
        EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(),
            reason: format!("invalid Ed25519 signature bytes: {e}"),
        }
    })?;

    // Look up signer in trust store
    let fingerprint = &header.signer_fingerprint;
    match trust_store.get(fingerprint) {
        Some(signer) => {
            // Verify: signature is over the content_hash hex string
            signer
                .verifying_key
                .verify(header.content_hash.as_bytes(), &signature)
                .map_err(|_| EngineError::SignatureVerificationFailed {
                    canonical_ref: String::new(),
                    reason: "Ed25519 signature verification failed".into(),
                })?;

            Ok((
                TrustClass::Trusted,
                Some(SignerFingerprint(fingerprint.clone())),
            ))
        }
        None => {
            // Signer not in trust store — we can't verify the signature
            // cryptographically, but the content hash check passed.
            // Mark as Untrusted.
            Ok((
                TrustClass::Untrusted,
                Some(SignerFingerprint(fingerprint.clone())),
            ))
        }
    }
}

/// Full item verification: takes a ResolvedItem, reads its content,
/// and produces a VerifiedItem.
pub fn verify_resolved_item(
    item: ResolvedItem,
    trust_store: &TrustStore,
) -> Result<VerifiedItem, EngineError> {
    let content = std::fs::read_to_string(&item.source_path).map_err(|e| {
        EngineError::Internal(format!(
            "failed to read {} for verification: {e}",
            item.source_path.display()
        ))
    })?;

    tracing::debug!(
        item_ref = %item.canonical_ref,
        has_signature = item.signature_header.is_some(),
        "verifying resolved item"
    );

    match &item.signature_header {
        Some(header) => {
            let (trust_class, signer) = verify_item_signature(
                &content,
                header,
                &item.source_format.signature,
                trust_store,
            )
            .map_err(|e| patch_canonical_ref(e, &item.canonical_ref.to_string()))?;

            Ok(VerifiedItem {
                resolved: item,
                signer,
                trust_class,
                pinned_version: None,
            })
        }
        None => {
            // No signature — Unsigned
            Ok(VerifiedItem {
                resolved: item,
                signer: None,
                trust_class: TrustClass::Unsigned,
                pinned_version: None,
            })
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in hash.iter() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// Patch the canonical_ref field into verification errors that were
/// created with an empty string placeholder.
fn patch_canonical_ref(err: EngineError, canonical_ref: &str) -> EngineError {
    match err {
        EngineError::SignatureVerificationFailed { reason, .. } => {
            EngineError::SignatureVerificationFailed {
                canonical_ref: canonical_ref.to_owned(),
                reason,
            }
        }
        EngineError::ContentHashMismatch {
            expected, actual, ..
        } => EngineError::ContentHashMismatch {
            canonical_ref: canonical_ref.to_owned(),
            expected,
            actual,
        },
        other => other,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::SignatureEnvelope;
    use ed25519_dalek::{Signer, SigningKey};
    use std::fs;

    fn hash_prefix_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: false,
        }
    }

    fn html_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "<!--".to_owned(),
            suffix: Some("-->".to_owned()),
            after_shebang: false,
        }
    }

    fn shebang_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: true,
        }
    }

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir = std::env::temp_dir().join(format!(
            "rye_trust_test_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Generate a signing key pair and return (signing_key, verifying_key, fingerprint).
    fn gen_key() -> (SigningKey, VerifyingKey, String) {
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = compute_fingerprint(&verifying_key);
        (signing_key, verifying_key, fingerprint)
    }

    /// Generate a second, distinct key pair.
    fn gen_key2() -> (SigningKey, VerifyingKey, String) {
        let signing_key = SigningKey::from_bytes(&[99u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = compute_fingerprint(&verifying_key);
        (signing_key, verifying_key, fingerprint)
    }

    /// Build a properly signed file content string.
    fn build_signed_content(
        body: &str,
        signing_key: &SigningKey,
        fingerprint: &str,
        envelope: &SignatureEnvelope,
    ) -> String {
        let content_hash = sha256_hex(body.as_bytes());
        let signature: Signature = signing_key.sign(content_hash.as_bytes());
        let sig_b64 =
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let timestamp = "2026-04-10T00:00:00Z";

        let payload = format!("rye:signed:{timestamp}:{content_hash}:{sig_b64}:{fingerprint}");

        match &envelope.suffix {
            Some(suffix) => format!("{} {payload} {suffix}\n{body}", envelope.prefix),
            None => format!("{} {payload}\n{body}", envelope.prefix),
        }
    }

    /// Build a properly signed file with shebang.
    fn build_signed_content_with_shebang(
        body: &str,
        signing_key: &SigningKey,
        fingerprint: &str,
        shebang: &str,
    ) -> String {
        let envelope = shebang_envelope();
        // Content after signature line = body
        // But with shebang, the signature line is line 2.
        // Content hash is over everything after the signature line.
        let content_hash = sha256_hex(body.as_bytes());
        let signature: Signature = signing_key.sign(content_hash.as_bytes());
        let sig_b64 =
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let timestamp = "2026-04-10T00:00:00Z";

        let payload = format!("rye:signed:{timestamp}:{content_hash}:{sig_b64}:{fingerprint}");
        format!("{shebang}\n{} {payload}\n{body}", envelope.prefix)
    }

    // ── TrustStore tests ────────────────────────────────────────────

    #[test]
    fn empty_trust_store() {
        let store = TrustStore::empty();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(!store.is_trusted("anything"));
    }

    #[test]
    fn trust_store_from_signers() {
        let (_, vk, fp) = gen_key();
        let signer = TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key: vk,
            label: Some("test".into()),
        };
        let store = TrustStore::from_signers(vec![signer]);
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        assert!(!store.is_trusted("other"));
    }

    #[test]
    fn trust_store_load_from_dir_empty() {
        let dir = tempdir();
        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn trust_store_load_from_dir_nonexistent() {
        let dir = tempdir().join("nonexistent");
        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn trust_store_load_ed25519_prefixed_key() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();
        let key_b64 =
            base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        fs::write(dir.join("signer1.pub"), format!("ed25519:{key_b64}\n")).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        let loaded = store.get(&fp).unwrap();
        assert_eq!(loaded.label.as_deref(), Some("signer1"));
    }

    #[test]
    fn trust_store_load_raw_b64_key() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();
        let key_b64 =
            base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        fs::write(dir.join("signer1.pub"), format!("{key_b64}\n")).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
    }

    #[test]
    fn trust_store_load_multiple_keys() {
        let dir = tempdir();
        let (_, vk1, fp1) = gen_key();
        let (_, vk2, fp2) = gen_key2();

        let b1 = base64::engine::general_purpose::STANDARD.encode(vk1.as_bytes());
        let b2 = base64::engine::general_purpose::STANDARD.encode(vk2.as_bytes());

        fs::write(dir.join("signer_a.pub"), format!("ed25519:{b1}\n")).unwrap();
        fs::write(dir.join("signer_b.pub"), format!("ed25519:{b2}\n")).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.is_trusted(&fp1));
        assert!(store.is_trusted(&fp2));
    }

    #[test]
    fn trust_store_errors_on_bad_key_file() {
        let dir = tempdir();
        let (_, vk, _) = gen_key();
        let key_b64 =
            base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        fs::write(dir.join("good.pub"), format!("ed25519:{key_b64}\n")).unwrap();
        fs::write(dir.join("bad.pub"), "not a valid key\n").unwrap();

        let err = TrustStore::load_from_dir(&dir).unwrap_err();
        assert!(
            matches!(err, EngineError::Internal(_)),
            "expected Internal error for bad .pub file, got: {err:?}"
        );
    }

    #[test]
    fn trust_store_skips_non_key_files() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();
        let key_b64 =
            base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        fs::write(dir.join("good.pub"), format!("ed25519:{key_b64}\n")).unwrap();
        fs::write(dir.join("readme.txt"), "this is not a key file").unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
    }

    // ── Content hash tests ──────────────────────────────────────────

    #[test]
    fn content_hash_after_sig_hash_prefix() {
        let body = "print('hello')\n";
        let content = format!("# rye:signed:2026-04-10T00:00:00Z:abc:sig:fp\n{body}");
        let envelope = hash_prefix_envelope();

        let hash = content_hash_after_signature(&content, &envelope).unwrap();
        assert_eq!(hash, sha256_hex(body.as_bytes()));
    }

    #[test]
    fn content_hash_after_sig_html_prefix() {
        let body = "# Hello\n";
        let content = format!("<!-- rye:signed:2026-04-10T00:00:00Z:abc:sig:fp -->\n{body}");
        let envelope = html_envelope();

        let hash = content_hash_after_signature(&content, &envelope).unwrap();
        assert_eq!(hash, sha256_hex(body.as_bytes()));
    }

    #[test]
    fn content_hash_after_sig_with_shebang() {
        let body = "print('hello')\n";
        let content =
            format!("#!/usr/bin/env python3\n# rye:signed:2026-04-10T00:00:00Z:abc:sig:fp\n{body}");
        let envelope = shebang_envelope();

        let hash = content_hash_after_signature(&content, &envelope).unwrap();
        assert_eq!(hash, sha256_hex(body.as_bytes()));
    }

    #[test]
    fn content_hash_no_signature_line() {
        let content = "print('hello')\n";
        let envelope = hash_prefix_envelope();

        assert!(content_hash_after_signature(content, &envelope).is_none());
    }

    // ── Signature verification tests ────────────────────────────────

    #[test]
    fn verify_valid_signature_trusted() {
        let (sk, vk, fp) = gen_key();
        let body = "print('hello')\n";
        let envelope = hash_prefix_envelope();
        let content = build_signed_content(body, &sk, &fp, &envelope);

        // Parse the header from the content
        let header = crate::resolution::parse_signature_header(&content, &envelope).unwrap();

        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key: vk,
            label: None,
        }]);

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store).unwrap();
        assert_eq!(trust_class, TrustClass::Trusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn verify_valid_signature_untrusted_signer() {
        let (sk, _, fp) = gen_key();
        let body = "print('hello')\n";
        let envelope = hash_prefix_envelope();
        let content = build_signed_content(body, &sk, &fp, &envelope);

        let header = crate::resolution::parse_signature_header(&content, &envelope).unwrap();

        // Empty trust store — signer not trusted
        let store = TrustStore::empty();

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store).unwrap();
        assert_eq!(trust_class, TrustClass::Untrusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn verify_tampered_content_fails() {
        let (sk, vk, fp) = gen_key();
        let body = "print('hello')\n";
        let envelope = hash_prefix_envelope();
        let mut content = build_signed_content(body, &sk, &fp, &envelope);

        // Tamper with the body
        content.push_str("# injected malicious code\n");

        let header = crate::resolution::parse_signature_header(&content, &envelope).unwrap();

        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp,
            verifying_key: vk,
            label: None,
        }]);

        let err = verify_item_signature(&content, &header, &envelope, &store).unwrap_err();
        assert!(
            matches!(err, EngineError::ContentHashMismatch { .. }),
            "expected ContentHashMismatch, got: {err:?}"
        );
    }

    #[test]
    fn verify_wrong_signer_key_fails() {
        let (sk, _, fp) = gen_key();
        let (_, vk2, _) = gen_key2();
        let body = "print('hello')\n";
        let envelope = hash_prefix_envelope();
        let content = build_signed_content(body, &sk, &fp, &envelope);

        let header = crate::resolution::parse_signature_header(&content, &envelope).unwrap();

        // Trust store has the fingerprint but mapped to a DIFFERENT key
        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp,
            verifying_key: vk2,
            label: None,
        }]);

        let err = verify_item_signature(&content, &header, &envelope, &store).unwrap_err();
        assert!(
            matches!(err, EngineError::SignatureVerificationFailed { .. }),
            "expected SignatureVerificationFailed, got: {err:?}"
        );
    }

    #[test]
    fn verify_html_envelope_signature() {
        let (sk, vk, fp) = gen_key();
        let body = "# My Directive\n\nSome content.\n";
        let envelope = html_envelope();
        let content = build_signed_content(body, &sk, &fp, &envelope);

        let header = crate::resolution::parse_signature_header(&content, &envelope).unwrap();

        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key: vk,
            label: None,
        }]);

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store).unwrap();
        assert_eq!(trust_class, TrustClass::Trusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn verify_shebang_envelope_signature() {
        let (sk, vk, fp) = gen_key();
        let body = "print('hello')\n";
        let content =
            build_signed_content_with_shebang(body, &sk, &fp, "#!/usr/bin/env python3");

        let envelope = shebang_envelope();
        let header = crate::resolution::parse_signature_header(&content, &envelope).unwrap();

        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key: vk,
            label: None,
        }]);

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store).unwrap();
        assert_eq!(trust_class, TrustClass::Trusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn fingerprint_matches_identity_rs_computation() {
        // The fingerprint should be SHA-256 of the raw 32-byte public key
        let (_, vk, fp) = gen_key();
        let expected = sha256_hex(vk.as_bytes());
        assert_eq!(fp, expected);
    }
}
