//! Item signer trust store and signature verification.
//!
//! Loads trusted signer public keys from `.ai/config/keys/trusted/` across
//! the three-tier space (project > user > system).
//! Supports two key file formats:
//!   - Raw `.pub`/`.key` files: base64-encoded 32-byte Ed25519 keys
//!   - Signed `.toml` identity docs: structured TOML with PEM public keys
//!
//! The TOML format matches the Python `TrustStore` identity documents:
//! ```toml
//! # rye:signed:TIMESTAMP:HASH:SIG:FP
//! fingerprint = "16e73c5829f69d6f..."
//! owner = "leo"
//! attestation = ""
//! version = "1.0.0"
//!
//! [public_key]
//! pem = """
//! -----BEGIN PUBLIC KEY-----
//! MCowBQYDK2VwAyEA...
//! -----END PUBLIC KEY-----
//! """
//! ```
//!
//! Verifies Ed25519 item signatures and computes content hashes over
//! the post-signature-line content.
//!
//! The trust store is a simple key-value map: signer fingerprint → public key.
//! It does NOT share trust policy with daemon request auth or node auth —
//! those are distinct trust domains (see 05-trust-auth-and-signatures.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use lillux::crypto::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;

use crate::contracts::{
    ResolvedItem, SignatureEnvelope, SignatureHeader, SignerFingerprint, TrustClass, VerifiedItem,
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

// ── Trusted key document (TOML format) ──────────────────────────────

/// A trusted key identity document parsed from a `.toml` file.
///
/// Mirrors the Python `TrustedKeyInfo` dataclass from
/// `ryeos/rye/utils/trust_store.py`.
#[derive(Debug, Clone)]
pub struct TrustedKeyDoc {
    pub fingerprint: String,
    pub owner: String,
    pub version: String,
    pub attestation: Option<String>,
    pub verifying_key: VerifyingKey,
}

/// Raw TOML structure for deserialization.
#[derive(Debug, Deserialize)]
struct TrustedKeyToml {
    fingerprint: String,
    #[serde(default = "default_owner")]
    owner: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    attestation: Option<String>,
    public_key: PublicKeySection,
}

#[derive(Debug, Deserialize)]
struct PublicKeySection {
    /// PEM-encoded Ed25519 public key, or `ed25519:<base64>` format
    pem: String,
}

fn default_owner() -> String {
    "unknown".to_string()
}

impl TrustedKeyDoc {
    /// Serialize to TOML body string (without signature line).
    pub fn to_toml(&self) -> String {
        let key_b64 =
            base64::engine::general_purpose::STANDARD.encode(self.verifying_key.as_bytes());
        let attestation = self.attestation.as_deref().unwrap_or("");
        format!(
            "fingerprint = \"{fp}\"\n\
             owner = \"{owner}\"\n\
             version = \"{version}\"\n\
             attestation = \"{attestation}\"\n\
             \n\
             [public_key]\n\
             pem = \"ed25519:{key_b64}\"\n",
            fp = self.fingerprint,
            owner = self.owner,
            version = self.version,
        )
    }
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
    /// Supports three file types:
    ///   - `.toml` — signed TOML identity documents (preferred format)
    ///   - `.pub` / `.key` — raw base64-encoded 32-byte Ed25519 public keys
    ///
    /// Files with other extensions are silently skipped.
    /// Bad `.pub`/`.key`/`.toml` files produce hard errors.
    ///
    /// The fingerprint is the SHA-256 hex digest of the raw public key bytes.
    pub fn load_from_dir(keys_dir: &Path) -> Result<Self, EngineError> {
        if !keys_dir.exists() {
            return Ok(Self::empty());
        }

        let entries = std::fs::read_dir(keys_dir).map_err(|e| {
            EngineError::Internal(format!(
                "cannot read trust store dir {}: {e}",
                keys_dir.display()
            ))
        })?;

        let mut signers = HashMap::new();

        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        paths.sort();

        for path in &paths {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

            match ext {
                "toml" => {
                    let doc = load_trusted_key_doc(path, &signers)?;
                    tracing::debug!(
                        fingerprint = %doc.fingerprint,
                        owner = %doc.owner,
                        path = %path.display(),
                        "loaded trusted key document"
                    );
                    signers.insert(
                        doc.fingerprint.clone(),
                        TrustedSigner {
                            fingerprint: doc.fingerprint,
                            verifying_key: doc.verifying_key,
                            label: Some(doc.owner),
                        },
                    );
                }
                "pub" | "key" => match load_signer_key(path) {
                    Ok(signer) => {
                        tracing::debug!(fingerprint = %signer.fingerprint, path = %path.display(), "loaded trusted signer key");
                        signers.insert(signer.fingerprint.clone(), signer);
                    }
                    Err(e) => return Err(e),
                },
                _ => {
                    // Non-key files (readme.txt, etc.) are silently skipped
                    continue;
                }
            }
        }

        Ok(Self { signers })
    }

    /// Load trusted keys with three-tier resolution: project > user > system.
    ///
    /// Each root is expected to contain `.ai/config/keys/trusted/` with
    /// `.toml` and/or `.pub`/`.key` files. First match wins — a key present
    /// in the project space is not overridden by user or system.
    pub fn load_three_tier(
        project_root: Option<&Path>,
        user_root: Option<&Path>,
        system_roots: &[PathBuf],
    ) -> Result<Self, EngineError> {
        let mut signers = HashMap::new();
        let trust_subdir = Path::new(crate::AI_DIR).join(crate::TRUST_KEYS_DIR);

        // Collect dirs in resolution order: project > user > system
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(root) = project_root {
            dirs.push(root.join(&trust_subdir));
        }
        if let Some(root) = user_root {
            dirs.push(root.join(&trust_subdir));
        }
        for root in system_roots {
            dirs.push(root.join(&trust_subdir));
        }

        for dir in &dirs {
            if !dir.is_dir() {
                continue;
            }
            let partial = Self::load_from_dir(dir)?;
            for (fp, signer) in partial.signers {
                // First match wins — don't override higher-priority spaces
                signers.entry(fp).or_insert(signer);
            }
        }

        let count = signers.len();
        if count > 0 {
            tracing::info!(count, dirs = dirs.len(), "loaded trust store (three-tier)");
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

    /// Return a new trust store that includes project-local keys.
    ///
    /// Project keys take priority (first-wins): if a fingerprint exists
    /// in the project trust dir, it shadows the same fingerprint in this
    /// store. Keys in this store that don't conflict are preserved.
    pub fn with_project_keys(&self, project_root: &Path) -> Result<Self, EngineError> {
        let trust_dir = project_root.join(crate::AI_DIR).join(crate::TRUST_KEYS_DIR);
        if !trust_dir.is_dir() {
            return Ok(self.clone());
        }
        let project_keys = Self::load_from_dir(&trust_dir)?;
        if project_keys.is_empty() {
            return Ok(self.clone());
        }
        let mut merged = project_keys.signers;
        for (fp, signer) in &self.signers {
            merged.entry(fp.clone()).or_insert_with(|| signer.clone());
        }
        Ok(Self { signers: merged })
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

// ── TOML key document loading ────────────────────────────────────────

/// Load a trusted key from a signed TOML identity document.
///
/// Format:
/// ```text
/// # rye:signed:TIMESTAMP:HASH:SIG:FP
/// fingerprint = "..."
/// owner = "..."
/// [public_key]
/// pem = """
/// -----BEGIN PUBLIC KEY-----
/// ...
/// -----END PUBLIC KEY-----
/// """
/// ```
///
/// Performs integrity verification when a signature line is present:
/// - Self-signed (signer_fp == key_fp): verifies using the key in the file
/// - Cross-signed (signer_fp != key_fp): looks up signer in already-loaded signers
/// - Unsigned files are accepted with a warning log
fn load_trusted_key_doc(
    path: &Path,
    existing_signers: &HashMap<String, TrustedSigner>,
) -> Result<TrustedKeyDoc, EngineError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        EngineError::Internal(format!(
            "cannot read trusted key doc {}: {e}",
            path.display()
        ))
    })?;

    let toml_body = lillux::signature::strip_signature_lines(&content);

    let parsed: TrustedKeyToml = toml::from_str(&toml_body).map_err(|e| {
        EngineError::Internal(format!(
            "invalid TOML in trusted key doc {}: {e}",
            path.display()
        ))
    })?;

    // Parse the public key — supports PEM or ed25519:<base64> format
    let verifying_key = parse_public_key_field(&parsed.public_key.pem, path)?;

    // Compute actual fingerprint and verify it matches the declared one
    let actual_fp = compute_fingerprint(&verifying_key);
    if actual_fp != parsed.fingerprint {
        return Err(EngineError::Internal(format!(
            "fingerprint mismatch in {}: declared {}, actual {}",
            path.display(),
            parsed.fingerprint,
            actual_fp,
        )));
    }

    // Verify integrity if signature line is present
    verify_key_doc_integrity(&content, &verifying_key, &actual_fp, existing_signers, path)?;

    let attestation = parsed.attestation.filter(|a| !a.is_empty());

    Ok(TrustedKeyDoc {
        fingerprint: actual_fp,
        owner: parsed.owner,
        version: parsed.version.unwrap_or_else(|| "1.0.0".to_string()),
        attestation,
        verifying_key,
    })
}

/// Parse a public key from the `[public_key].pem` field.
///
/// Accepts two formats:
///   - PKCS#8 PEM: `-----BEGIN PUBLIC KEY-----\n...\n-----END PUBLIC KEY-----`
///   - Raw format: `ed25519:<base64>` (32-byte key)
fn parse_public_key_field(pem_str: &str, path: &Path) -> Result<VerifyingKey, EngineError> {
    let trimmed = pem_str.trim();

    // Try ed25519:<base64> format first (simpler, used by daemon auth)
    if let Some(b64) = trimmed.strip_prefix("ed25519:") {
        let key_bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| {
                EngineError::Internal(format!(
                    "invalid base64 in ed25519: key in {}: {e}",
                    path.display()
                ))
            })?;
        let key_array: [u8; 32] = key_bytes.try_into().map_err(|_| {
            EngineError::Internal(format!(
                "ed25519: key in {} must be 32 bytes",
                path.display()
            ))
        })?;
        return VerifyingKey::from_bytes(&key_array).map_err(|e| {
            EngineError::Internal(format!("invalid Ed25519 key in {}: {e}", path.display()))
        });
    }

    // Try PKCS#8 PEM format
    if trimmed.contains("-----BEGIN PUBLIC KEY-----") {
        use lillux::crypto::DecodePublicKey;
        return VerifyingKey::from_public_key_pem(trimmed).map_err(|e| {
            EngineError::Internal(format!("invalid PEM public key in {}: {e}", path.display()))
        });
    }

    // Fall back to raw base64 (32 bytes)
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|e| {
            EngineError::Internal(format!(
                "invalid base64 in public key in {}: {e}",
                path.display()
            ))
        })?;
    let key_array: [u8; 32] = key_bytes.try_into().map_err(|_| {
        EngineError::Internal(format!("public key in {} must be 32 bytes", path.display()))
    })?;
    VerifyingKey::from_bytes(&key_array).map_err(|e| {
        EngineError::Internal(format!(
            "invalid Ed25519 public key in {}: {e}",
            path.display()
        ))
    })
}

/// Verify the integrity of a signed TOML key document.
///
/// Parses the `# rye:signed:TIMESTAMP:HASH:SIG:FP` line (if present),
/// recomputes the content hash, and verifies the Ed25519 signature.
///
/// Supports self-signed docs (signer == key in file) and cross-signed
/// docs (signer is another key already loaded in the trust store).
fn verify_key_doc_integrity(
    content: &str,
    key_in_file: &VerifyingKey,
    key_fingerprint: &str,
    existing_signers: &HashMap<String, TrustedSigner>,
    path: &Path,
) -> Result<(), EngineError> {
    // Find the signature line
    let sig_line = match content.lines().find(|l| l.starts_with("# rye:signed:")) {
        Some(line) => line,
        None => {
            tracing::debug!(path = %path.display(), "unsigned trusted key document");
            return Ok(());
        }
    };

    // Parse: # rye:signed:<timestamp>:<content_hash>:<sig_b64>:<signer_fp>
    // Timestamp may contain colons, so rsplit from the right
    let remainder = &sig_line["# rye:signed:".len()..];
    let parts: Vec<&str> = remainder.rsplitn(4, ':').collect();
    if parts.len() != 4 {
        return Err(EngineError::Internal(format!(
            "malformed signature header in {}",
            path.display()
        )));
    }
    let signer_fp = parts[0];
    let sig_b64 = parts[1];
    let claimed_hash = parts[2];

    // Compute content hash over everything after the signature line
    let sig_line_with_newline = format!("{sig_line}\n");
    let after_sig = content
        .strip_prefix(&sig_line_with_newline)
        .unwrap_or_else(|| {
            // Signature line without trailing newline (EOF)
            content.strip_prefix(sig_line).unwrap_or("")
        });
    let actual_hash = sha256_hex(after_sig.as_bytes());

    if actual_hash != claimed_hash {
        return Err(EngineError::Internal(format!(
            "content tampered in {}: expected {}…, got {}…",
            path.display(),
            &claimed_hash[..claimed_hash.len().min(16)],
            &actual_hash[..actual_hash.len().min(16)],
        )));
    }

    // Decode signature
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig_b64)
        .map_err(|e| {
            EngineError::Internal(format!(
                "invalid signature base64 in {}: {e}",
                path.display()
            ))
        })?;
    let signature = Signature::from_slice(&sig_bytes).map_err(|e| {
        EngineError::Internal(format!(
            "invalid Ed25519 signature in {}: {e}",
            path.display()
        ))
    })?;

    // Determine which key to verify with
    if signer_fp == key_fingerprint {
        // Self-signed: verify using the key described in the file
        key_in_file
            .verify(claimed_hash.as_bytes(), &signature)
            .map_err(|_| {
                EngineError::Internal(format!(
                    "self-signed signature invalid in {}",
                    path.display()
                ))
            })?;
    } else {
        // Cross-signed: look up the signing key in already-loaded signers
        match existing_signers.get(signer_fp) {
            Some(signer) => {
                signer
                    .verifying_key
                    .verify(claimed_hash.as_bytes(), &signature)
                    .map_err(|_| {
                        EngineError::Internal(format!(
                            "cross-signed signature invalid in {}",
                            path.display()
                        ))
                    })?;
            }
            None => {
                return Err(EngineError::Internal(format!(
                    "signing key {} not found in trust store for {}",
                    signer_fp,
                    path.display()
                )));
            }
        }
    }

    Ok(())
}

// ── TOFU key pinning ────────────────────────────────────────────────

/// Pin a public key to the trust store by writing a signed TOML doc.
///
/// Idempotent: no-op if `{target_dir}/{fingerprint}.toml` already exists.
///
/// If `signing_key` is provided, the TOML doc is self-signed by the
/// pinned key (standard TOFU pattern). Otherwise it is written unsigned.
///
/// Returns the fingerprint of the pinned key.
pub fn pin_key(
    verifying_key: &VerifyingKey,
    owner: &str,
    target_dir: &Path,
    signing_key: Option<&lillux::crypto::SigningKey>,
) -> Result<String, EngineError> {
    let fingerprint = compute_fingerprint(verifying_key);
    let key_file = target_dir.join(format!("{fingerprint}.toml"));

    // Idempotent — already pinned
    if key_file.exists() {
        tracing::debug!(fingerprint = %fingerprint, "key already pinned, skipping");
        return Ok(fingerprint);
    }

    let doc = TrustedKeyDoc {
        fingerprint: fingerprint.clone(),
        owner: owner.to_string(),
        version: "1.0.0".to_string(),
        attestation: None,
        verifying_key: *verifying_key,
    };
    let body = doc.to_toml();

    let content = match signing_key {
        Some(sk) => sign_toml_doc(&body, sk),
        None => body,
    };

    // Atomic write via temp file
    std::fs::create_dir_all(target_dir).map_err(|e| {
        EngineError::Internal(format!(
            "cannot create trust dir {}: {e}",
            target_dir.display()
        ))
    })?;
    let tmp = key_file.with_extension("tmp");
    std::fs::write(&tmp, &content).map_err(|e| {
        EngineError::Internal(format!("cannot write trust key {}: {e}", tmp.display()))
    })?;
    std::fs::rename(&tmp, &key_file).map_err(|e| {
        EngineError::Internal(format!(
            "cannot rename {} → {}: {e}",
            tmp.display(),
            key_file.display()
        ))
    })?;

    tracing::info!(fingerprint = %fingerprint, owner = %owner, "pinned trusted key");
    Ok(fingerprint)
}

/// Prepend a `# rye:signed:...` line to a TOML document body.
fn sign_toml_doc(body: &str, signing_key: &lillux::crypto::SigningKey) -> String {
    lillux::signature::sign_content(body, signing_key, "#", None)
}

// ── Fingerprint computation ─────────────────────────────────────────

/// Compute the SHA-256 hex fingerprint of an Ed25519 public key.
///
/// This matches the fingerprint computation in `ryeosd/src/identity.rs`.
pub fn compute_fingerprint(key: &VerifyingKey) -> String {
    lillux::signature::compute_fingerprint(key)
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
pub fn content_hash_after_signature(content: &str, envelope: &SignatureEnvelope) -> Option<String> {
    lillux::signature::content_hash_after_signature(
        content,
        &envelope.prefix,
        envelope.suffix.as_deref(),
        envelope.after_shebang,
    )
}

pub fn strip_signature_lines(content: &str) -> String {
    lillux::signature::strip_signature_lines(content)
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
#[tracing::instrument(
    name = "engine:trust_verify",
    skip(content, trust_store),
    fields(signer = ?header.signer_fingerprint)
)]
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

    tracing::trace!(actual_hash = %actual_hash, header_hash = %header.content_hash, "comparing content hashes");

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

            tracing::trace!(fingerprint = %fingerprint, trust_class = "trusted", "signature verified");
            Ok((
                TrustClass::Trusted,
                Some(SignerFingerprint(fingerprint.clone())),
            ))
        }
        None => {
            // Signer not in trust store — we can't verify the signature
            // cryptographically, but the content hash check passed.
            // Mark as Untrusted.
            tracing::trace!(fingerprint = %fingerprint, trust_class = "untrusted", "signer not in trust store");
            Ok((
                TrustClass::Untrusted,
                Some(SignerFingerprint(fingerprint.clone())),
            ))
        }
    }
}

/// Full item verification: takes a ResolvedItem, reads its content,
/// and produces a VerifiedItem.
#[tracing::instrument(
    name = "engine:verify_item",
    skip(item, trust_store),
    fields(canonical_ref = %item.canonical_ref)
)]
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
            let (trust_class, signer) =
                verify_item_signature(&content, header, &item.source_format.signature, trust_store)
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
    lillux::cas::sha256_hex(data)
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
    use lillux::crypto::{Signer, SigningKey};
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
        let dir =
            std::env::temp_dir().join(format!("rye_trust_test_{}_{}", std::process::id(), nanos));
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
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
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
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
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
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

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
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

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
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

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
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        fs::write(dir.join("good.pub"), format!("ed25519:{key_b64}\n")).unwrap();
        fs::write(dir.join("readme.txt"), "this is not a key file").unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
    }

    // ── Three-tier resolution tests ────────────────────────────────

    #[test]
    fn three_tier_project_overrides_user() {
        let project = tempdir();
        let user = tempdir();

        let trust_subdir = Path::new(crate::AI_DIR).join(crate::TRUST_KEYS_DIR);
        let project_trust = project.join(&trust_subdir);
        let user_trust = user.join(&trust_subdir);
        fs::create_dir_all(&project_trust).unwrap();
        fs::create_dir_all(&user_trust).unwrap();

        let (_, vk, fp) = gen_key();

        // Same fingerprint in both spaces, different labels
        let project_toml = build_key_doc_toml(&fp, "project_owner", &vk);
        let user_toml = build_key_doc_toml(&fp, "user_owner", &vk);
        fs::write(project_trust.join(format!("{fp}.toml")), &project_toml).unwrap();
        fs::write(user_trust.join(format!("{fp}.toml")), &user_toml).unwrap();

        let store = TrustStore::load_three_tier(Some(&project), Some(&user), &[]).unwrap();

        assert_eq!(store.len(), 1);
        // Project wins — label should be "project_owner"
        assert_eq!(
            store.get(&fp).unwrap().label.as_deref(),
            Some("project_owner")
        );
    }

    #[test]
    fn three_tier_merges_across_spaces() {
        let project = tempdir();
        let user = tempdir();

        let trust_subdir = Path::new(crate::AI_DIR).join(crate::TRUST_KEYS_DIR);
        let project_trust = project.join(&trust_subdir);
        let user_trust = user.join(&trust_subdir);
        fs::create_dir_all(&project_trust).unwrap();
        fs::create_dir_all(&user_trust).unwrap();

        let (_, vk1, fp1) = gen_key();
        let (_, vk2, fp2) = gen_key2();

        // Key 1 in project only
        let t1 = build_key_doc_toml(&fp1, "alice", &vk1);
        fs::write(project_trust.join(format!("{fp1}.toml")), &t1).unwrap();

        // Key 2 in user only
        let t2 = build_key_doc_toml(&fp2, "bob", &vk2);
        fs::write(user_trust.join(format!("{fp2}.toml")), &t2).unwrap();

        let store = TrustStore::load_three_tier(Some(&project), Some(&user), &[]).unwrap();

        assert_eq!(store.len(), 2);
        assert!(store.is_trusted(&fp1));
        assert!(store.is_trusted(&fp2));
    }

    #[test]
    fn three_tier_empty_when_no_dirs_exist() {
        let store = TrustStore::load_three_tier(
            Some(Path::new("/nonexistent/project")),
            Some(Path::new("/nonexistent/user")),
            &[],
        )
        .unwrap();
        assert!(store.is_empty());
    }

    // ── TOML key document tests ────────────────────────────────────

    /// Build a TOML trusted key document body (without signature line).
    fn build_key_doc_toml(fingerprint: &str, owner: &str, vk: &VerifyingKey) -> String {
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        format!(
            r#"fingerprint = "{fingerprint}"
owner = "{owner}"
version = "1.0.0"
attestation = ""

[public_key]
pem = "ed25519:{key_b64}"
"#
        )
    }

    /// Build a self-signed TOML trusted key document.
    fn build_signed_key_doc(sk: &SigningKey, vk: &VerifyingKey, fp: &str, owner: &str) -> String {
        let body = build_key_doc_toml(fp, owner, vk);
        let content_hash = sha256_hex(body.as_bytes());
        let signature: Signature = sk.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let timestamp = "2026-04-10T00:00:00Z";
        format!("# rye:signed:{timestamp}:{content_hash}:{sig_b64}:{fp}\n{body}")
    }

    #[test]
    fn trust_store_load_unsigned_toml_key() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();

        let toml_content = build_key_doc_toml(&fp, "alice", &vk);
        fs::write(dir.join(format!("{fp}.toml")), &toml_content).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        let loaded = store.get(&fp).unwrap();
        assert_eq!(loaded.label.as_deref(), Some("alice"));
    }

    #[test]
    fn trust_store_load_self_signed_toml_key() {
        let dir = tempdir();
        let (sk, vk, fp) = gen_key();

        let content = build_signed_key_doc(&sk, &vk, &fp, "bob");
        fs::write(dir.join(format!("{fp}.toml")), &content).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        assert_eq!(store.get(&fp).unwrap().label.as_deref(), Some("bob"));
    }

    #[test]
    fn trust_store_load_cross_signed_toml_key() {
        let dir = tempdir();
        let (sk1, vk1, fp1) = gen_key();
        let (_, vk2, fp2) = gen_key2();

        // First key is self-signed
        let content1 = build_signed_key_doc(&sk1, &vk1, &fp1, "signer");
        // Second key is cross-signed by first key
        let body2 = build_key_doc_toml(&fp2, "signee", &vk2);
        let content_hash = sha256_hex(body2.as_bytes());
        let signature: Signature = sk1.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let content2 =
            format!("# rye:signed:2026-04-10T00:00:00Z:{content_hash}:{sig_b64}:{fp1}\n{body2}");

        // Write files — sorted order matters: fp1 file must come before fp2
        // Use names that sort correctly
        fs::write(dir.join("a_signer.toml"), &content1).unwrap();
        fs::write(dir.join("b_signee.toml"), &content2).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.is_trusted(&fp1));
        assert!(store.is_trusted(&fp2));
    }

    #[test]
    fn trust_store_toml_tampered_content() {
        let dir = tempdir();
        let (sk, vk, fp) = gen_key();

        let mut content = build_signed_key_doc(&sk, &vk, &fp, "eve");
        // Tamper with the content
        content.push_str("# injected\n");
        fs::write(dir.join(format!("{fp}.toml")), &content).unwrap();

        let err = TrustStore::load_from_dir(&dir).unwrap_err();
        assert!(
            matches!(err, EngineError::Internal(ref msg) if msg.contains("content tampered")),
            "expected content tampered error, got: {err:?}"
        );
    }

    #[test]
    fn trust_store_toml_fingerprint_mismatch() {
        let dir = tempdir();
        let (_, vk, _fp) = gen_key();

        // Wrong fingerprint in the document
        let toml_content = build_key_doc_toml("wrong_fingerprint", "charlie", &vk);
        fs::write(dir.join("wrong.toml"), &toml_content).unwrap();

        let err = TrustStore::load_from_dir(&dir).unwrap_err();
        assert!(
            matches!(err, EngineError::Internal(ref msg) if msg.contains("fingerprint mismatch")),
            "expected fingerprint mismatch error, got: {err:?}"
        );
    }

    #[test]
    fn trust_store_toml_invalid_signature() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();
        let (sk2, _, _) = gen_key2();

        // Sign with wrong key but claim to be self-signed
        let body = build_key_doc_toml(&fp, "mallory", &vk);
        let content_hash = sha256_hex(body.as_bytes());
        let signature: Signature = sk2.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let content =
            format!("# rye:signed:2026-04-10T00:00:00Z:{content_hash}:{sig_b64}:{fp}\n{body}");
        fs::write(dir.join(format!("{fp}.toml")), &content).unwrap();

        let err = TrustStore::load_from_dir(&dir).unwrap_err();
        assert!(
            matches!(err, EngineError::Internal(ref msg) if msg.contains("signature invalid")),
            "expected signature invalid error, got: {err:?}"
        );
    }

    #[test]
    fn trust_store_mixed_toml_and_pub_files() {
        let dir = tempdir();
        let (_sk, vk1, fp1) = gen_key();
        let (_, vk2, fp2) = gen_key2();

        // One TOML key doc
        let toml_content = build_key_doc_toml(&fp1, "toml_user", &vk1);
        fs::write(dir.join(format!("{fp1}.toml")), &toml_content).unwrap();

        // One .pub raw key
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk2.as_bytes());
        fs::write(dir.join("other.pub"), format!("ed25519:{key_b64}\n")).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.is_trusted(&fp1));
        assert!(store.is_trusted(&fp2));
    }

    #[test]
    fn trust_store_toml_cross_sign_unknown_signer() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();
        let (sk2, _, fp2) = gen_key2();

        // Sign with sk2 but fp2 is not in the store
        let body = build_key_doc_toml(&fp, "orphan", &vk);
        let content_hash = sha256_hex(body.as_bytes());
        let signature: Signature = sk2.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let content =
            format!("# rye:signed:2026-04-10T00:00:00Z:{content_hash}:{sig_b64}:{fp2}\n{body}");
        fs::write(dir.join(format!("{fp}.toml")), &content).unwrap();

        let err = TrustStore::load_from_dir(&dir).unwrap_err();
        assert!(
            matches!(err, EngineError::Internal(ref msg) if msg.contains("not found in trust store")),
            "expected signing key not found error, got: {err:?}"
        );
    }

    // ── Pin key tests ────────────────────────────────────────────────

    #[test]
    fn pin_key_writes_unsigned_toml() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();

        let result_fp = pin_key(&vk, "alice", &dir, None).unwrap();
        assert_eq!(result_fp, fp);

        // File was created
        let key_file = dir.join(format!("{fp}.toml"));
        assert!(key_file.exists());

        // Loadable by trust store
        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        assert_eq!(store.get(&fp).unwrap().label.as_deref(), Some("alice"));
    }

    #[test]
    fn pin_key_writes_signed_toml() {
        let dir = tempdir();
        let (sk, vk, fp) = gen_key();

        let result_fp = pin_key(&vk, "bob", &dir, Some(&sk)).unwrap();
        assert_eq!(result_fp, fp);

        // File has signature line
        let content = fs::read_to_string(dir.join(format!("{fp}.toml"))).unwrap();
        assert!(content.starts_with("# rye:signed:"));

        // Loadable and passes integrity verification
        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
    }

    #[test]
    fn pin_key_idempotent() {
        let dir = tempdir();
        let (sk, vk, fp) = gen_key();

        pin_key(&vk, "first", &dir, Some(&sk)).unwrap();
        let content_before = fs::read_to_string(dir.join(format!("{fp}.toml"))).unwrap();

        // Second call is a no-op
        pin_key(&vk, "second", &dir, Some(&sk)).unwrap();
        let content_after = fs::read_to_string(dir.join(format!("{fp}.toml"))).unwrap();

        assert_eq!(content_before, content_after);
    }

    #[test]
    fn pin_key_creates_directory() {
        let base = tempdir();
        let nested = base.join("deep/nested/trust");
        let (_, vk, fp) = gen_key();

        pin_key(&vk, "charlie", &nested, None).unwrap();
        assert!(nested.join(format!("{fp}.toml")).exists());
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
        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

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

        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

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

        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

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

        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

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

        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

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
        let content = build_signed_content_with_shebang(body, &sk, &fp, "#!/usr/bin/env python3");

        let envelope = shebang_envelope();
        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

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
