//! Sealed-envelope vault primitives — X25519 + XChaCha20-Poly1305.
//!
//! ## Construction
//!
//! Every entry is wrapped in two layers:
//!
//! 1. **AEAD layer (data)**: the plaintext is encrypted with
//!    XChaCha20-Poly1305 under a random 256-bit data-encryption key
//!    (DEK) and a random 192-bit nonce.
//! 2. **DEK-wrap layer (key)**: the DEK is encrypted to the vault's
//!    long-lived X25519 public key using a libsodium-style sealed
//!    envelope. The wrapped DEK starts with the 32-byte ephemeral
//!    public key, followed by the AEAD ciphertext+tag.
//!
//! The result is a [`SealedEnvelope`] containing
//! `{ version, vault_pubkey_fingerprint, wrapped_dek, nonce, ciphertext }`.
//! Decryption requires the vault's X25519 secret key.
//!
//! ## Why these primitives
//!
//! - **X25519** for DEK wrap: small surface, no curve confusion, no
//!   parameter footguns. The recipient's identity is the public key.
//! - **XChaCha20-Poly1305** for the data layer: 192-bit random nonces
//!   are safe to generate without a counter, unlike AES-GCM. Tag is
//!   16 bytes (Poly1305).
//! - **No HKDF**: the sealed-DEK construction uses a deterministic key
//!   schedule keyed on `H(eph_pk || vault_pk || shared)` so a single
//!   wrapped DEK is reproducible from `(eph_pk, vault_pk)` — matches
//!   libsodium's `crypto_box_seal`.
//!
//! ## Format on disk (suggested)
//!
//! ```toml
//! version = 1
//! vault_pubkey_fingerprint = "..."
//! wrapped_dek = "<base64>"
//! nonce       = "<base64>"
//! ciphertext  = "<base64>"
//! ```
//!
//! Higher-level callers (e.g. `ryeosd/src/vault.rs`) decide what to
//! put inside the plaintext (a TOML map of secrets, a single value,
//! anything serializable). The envelope itself is opaque bytes-in,
//! bytes-out.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

const ENVELOPE_VERSION: u32 = 1;
const DEK_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const EPH_PK_LEN: usize = 32;
const DEK_WRAP_DOMAIN: &[u8] = b"ryeos-vault-v1-wrap";
const DEK_NONCE_DOMAIN: &[u8] = b"ryeos-vault-v1-wrap-nonce";

// ── Key types ────────────────────────────────────────────────────────

/// Long-lived X25519 secret key for vault decryption.
///
/// Wraps `x25519_dalek::StaticSecret` so callers don't have to import
/// the curve crate directly. Keys are generated once at `ryeos init`
/// time and persisted at `<state>/.ai/node/vault/private_key.pem`.
#[derive(Clone)]
pub struct VaultSecretKey(StaticSecret);

impl std::fmt::Debug for VaultSecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("VaultSecretKey").field(&"<redacted>").finish()
    }
}

/// X25519 public key. Distributable — operators may include this in
/// audit reports / docs without exposing the secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaultPublicKey(PublicKey);

impl VaultSecretKey {
    /// Generate a fresh keypair from the OS RNG.
    pub fn generate() -> Self {
        Self(StaticSecret::random_from_rng(OsRng))
    }

    /// Derive the matching public key.
    pub fn public_key(&self) -> VaultPublicKey {
        VaultPublicKey(PublicKey::from(&self.0))
    }

    /// Raw 32-byte representation. Persistence-only — handle with care.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// Reconstruct from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(StaticSecret::from(bytes))
    }
}

impl VaultPublicKey {
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(PublicKey::from(bytes))
    }

    /// SHA-256 hex digest of the raw 32-byte public key.
    pub fn fingerprint(&self) -> String {
        crate::sha256_hex(self.as_bytes())
    }
}

// ── Persistence helpers (raw base64, single line) ───────────────────
//
// We don't use PKCS#8 PEM here. PKCS#8 for X25519 has poor library
// support across the ecosystem and adds a non-trivial wrapping format
// the operator can't easily inspect with grep. Stick to a one-line
// `x25519:<base64>` representation matching the trust-doc convention.

const SECRET_TAG: &str = "x25519-secret:";
const PUBLIC_TAG: &str = "x25519-public:";

/// Encode a vault secret key to a single-line `x25519-secret:<b64>` string.
pub fn encode_secret_key(sk: &VaultSecretKey) -> String {
    format!(
        "{}{}",
        SECRET_TAG,
        base64::engine::general_purpose::STANDARD.encode(sk.to_bytes())
    )
}

/// Encode a vault public key to a single-line `x25519-public:<b64>` string.
pub fn encode_public_key(pk: &VaultPublicKey) -> String {
    format!(
        "{}{}",
        PUBLIC_TAG,
        base64::engine::general_purpose::STANDARD.encode(pk.as_bytes())
    )
}

/// Decode a vault secret key written by [`encode_secret_key`].
pub fn decode_secret_key(s: &str) -> Result<VaultSecretKey> {
    let trimmed = s.trim();
    let b64 = trimmed
        .strip_prefix(SECRET_TAG)
        .ok_or_else(|| anyhow!("missing `{SECRET_TAG}` prefix"))?
        .trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("base64 decode")?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("expected 32-byte X25519 secret key"))?;
    Ok(VaultSecretKey::from_bytes(arr))
}

/// Decode a vault public key written by [`encode_public_key`].
pub fn decode_public_key(s: &str) -> Result<VaultPublicKey> {
    let trimmed = s.trim();
    let b64 = trimmed
        .strip_prefix(PUBLIC_TAG)
        .ok_or_else(|| anyhow!("missing `{PUBLIC_TAG}` prefix"))?
        .trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("base64 decode")?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("expected 32-byte X25519 public key"))?;
    Ok(VaultPublicKey::from_bytes(arr))
}

/// Read a secret key from a file written by [`write_secret_key`].
pub fn read_secret_key(path: &Path) -> Result<VaultSecretKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    decode_secret_key(&raw).with_context(|| format!("parse {}", path.display()))
}

/// Atomically write a vault secret key. File mode 0600 on Unix.
pub fn write_secret_key(path: &Path, sk: &VaultSecretKey) -> Result<()> {
    let body = encode_secret_key(sk) + "\n";
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Atomically write a vault public key.
pub fn write_public_key(path: &Path, pk: &VaultPublicKey) -> Result<()> {
    let body = encode_public_key(pk) + "\n";
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Read a public key from a file.
pub fn read_public_key(path: &Path) -> Result<VaultPublicKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    decode_public_key(&raw).with_context(|| format!("parse {}", path.display()))
}

// ── Sealed envelope ─────────────────────────────────────────────────

/// Encrypted payload — versioned, self-describing, base64 fields for
/// safe TOML/JSON nesting. The envelope is opaque to the engine: it
/// only knows how to seal and open with a key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedEnvelope {
    pub version: u32,
    pub vault_pubkey_fingerprint: String,
    /// Base64 of `eph_pk(32) || aead_ciphertext(DEK_LEN + 16)`.
    pub wrapped_dek: String,
    /// Base64 of the data-layer XChaCha20-Poly1305 nonce (24 bytes).
    pub nonce: String,
    /// Base64 of the data-layer XChaCha20-Poly1305 ciphertext+tag.
    pub ciphertext: String,
}

/// Seal `plaintext` to the vault public key. Each call generates a
/// fresh DEK + ephemeral keypair, so two seals of the same plaintext
/// produce different envelopes (the nonce reuse problem is therefore
/// impossible in this construction).
pub fn seal(vault_pk: &VaultPublicKey, plaintext: &[u8]) -> Result<SealedEnvelope> {
    // 1. Generate fresh DEK + data nonce.
    let mut dek = [0u8; DEK_LEN];
    OsRng.fill_bytes(&mut dek);
    let mut data_nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut data_nonce);

    // 2. AEAD-encrypt the plaintext with the DEK.
    let aead = XChaCha20Poly1305::new(&dek.into());
    let ciphertext = aead
        .encrypt(XNonce::from_slice(&data_nonce), plaintext)
        .map_err(|e| anyhow!("XChaCha20Poly1305 encrypt failed: {e}"))?;

    // 3. Wrap the DEK to the vault public key.
    let wrapped = wrap_dek(&dek, vault_pk)?;

    Ok(SealedEnvelope {
        version: ENVELOPE_VERSION,
        vault_pubkey_fingerprint: vault_pk.fingerprint(),
        wrapped_dek: base64::engine::general_purpose::STANDARD.encode(&wrapped),
        nonce: base64::engine::general_purpose::STANDARD.encode(data_nonce),
        ciphertext: base64::engine::general_purpose::STANDARD.encode(&ciphertext),
    })
}

/// Open a [`SealedEnvelope`] with the vault secret key.
///
/// Refuses on:
///   - version mismatch
///   - fingerprint mismatch (envelope was sealed to a different vault)
///   - any AEAD failure (tampering or wrong key)
pub fn open(vault_sk: &VaultSecretKey, env: &SealedEnvelope) -> Result<Vec<u8>> {
    if env.version != ENVELOPE_VERSION {
        bail!(
            "vault: envelope version {} not supported (expected {})",
            env.version,
            ENVELOPE_VERSION
        );
    }
    let vault_pk = vault_sk.public_key();
    let our_fp = vault_pk.fingerprint();
    if env.vault_pubkey_fingerprint != our_fp {
        bail!(
            "vault: envelope sealed to fingerprint {} but our vault key is {} \
             — wrong key, or a `ryeos vault rewrap` is needed after rotation",
            env.vault_pubkey_fingerprint,
            our_fp
        );
    }

    let wrapped = base64::engine::general_purpose::STANDARD
        .decode(&env.wrapped_dek)
        .context("wrapped_dek base64")?;
    let dek = unwrap_dek(&wrapped, vault_sk)?;

    let data_nonce = base64::engine::general_purpose::STANDARD
        .decode(&env.nonce)
        .context("nonce base64")?;
    if data_nonce.len() != NONCE_LEN {
        bail!("vault: nonce must be {NONCE_LEN} bytes");
    }
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(&env.ciphertext)
        .context("ciphertext base64")?;

    let aead = XChaCha20Poly1305::new(&dek.into());
    aead.decrypt(XNonce::from_slice(&data_nonce), ciphertext.as_ref())
        .map_err(|_| anyhow!("vault: AEAD decryption failed (tampered envelope or wrong key)"))
}

// ── Sealed-DEK construction (libsodium-style sealed box) ────────────

/// Wrap `dek` such that only the holder of the vault secret key can
/// recover it. Output: `eph_pk(32) || aead_ciphertext_with_tag(48)`.
fn wrap_dek(dek: &[u8; DEK_LEN], vault_pk: &VaultPublicKey) -> Result<Vec<u8>> {
    let eph_sk = StaticSecret::random_from_rng(OsRng);
    let eph_pk = PublicKey::from(&eph_sk);
    let shared = eph_sk.diffie_hellman(&vault_pk.0);

    let key = derive_wrap_key(&eph_pk, vault_pk, shared.as_bytes());
    let nonce = derive_wrap_nonce(&eph_pk, vault_pk);

    let aead = XChaCha20Poly1305::new(&key.into());
    let ciphertext = aead
        .encrypt(XNonce::from_slice(&nonce), dek.as_ref())
        .map_err(|e| anyhow!("DEK wrap AEAD encrypt failed: {e}"))?;

    let mut out = Vec::with_capacity(EPH_PK_LEN + ciphertext.len());
    out.extend_from_slice(eph_pk.as_bytes());
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Recover a DEK wrapped by [`wrap_dek`] using the vault secret key.
fn unwrap_dek(wrapped: &[u8], vault_sk: &VaultSecretKey) -> Result<[u8; DEK_LEN]> {
    if wrapped.len() < EPH_PK_LEN {
        bail!("vault: wrapped_dek too short ({} bytes)", wrapped.len());
    }
    let mut eph_pk_bytes = [0u8; EPH_PK_LEN];
    eph_pk_bytes.copy_from_slice(&wrapped[..EPH_PK_LEN]);
    let eph_pk = PublicKey::from(eph_pk_bytes);
    let ciphertext = &wrapped[EPH_PK_LEN..];

    let shared = vault_sk.0.diffie_hellman(&eph_pk);
    let vault_pk = vault_sk.public_key();

    let key = derive_wrap_key(&eph_pk, &vault_pk, shared.as_bytes());
    let nonce = derive_wrap_nonce(&eph_pk, &vault_pk);

    let aead = XChaCha20Poly1305::new(&key.into());
    let dek_bytes = aead
        .decrypt(XNonce::from_slice(&nonce), ciphertext)
        .map_err(|_| anyhow!("vault: DEK unwrap failed (wrong key or tampered envelope)"))?;
    if dek_bytes.len() != DEK_LEN {
        bail!("vault: unwrapped DEK has wrong length: {}", dek_bytes.len());
    }
    let mut dek = [0u8; DEK_LEN];
    dek.copy_from_slice(&dek_bytes);
    Ok(dek)
}

fn derive_wrap_key(eph_pk: &PublicKey, vault_pk: &VaultPublicKey, shared: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(DEK_WRAP_DOMAIN);
    h.update(eph_pk.as_bytes());
    h.update(vault_pk.as_bytes());
    h.update(shared);
    let digest = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn derive_wrap_nonce(eph_pk: &PublicKey, vault_pk: &VaultPublicKey) -> [u8; NONCE_LEN] {
    let mut h = Sha256::new();
    h.update(DEK_NONCE_DOMAIN);
    h.update(eph_pk.as_bytes());
    h.update(vault_pk.as_bytes());
    let digest = h.finalize();
    let mut out = [0u8; NONCE_LEN];
    out.copy_from_slice(&digest[..NONCE_LEN]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_short_message() {
        let sk = VaultSecretKey::generate();
        let pk = sk.public_key();
        let env = seal(&pk, b"hello world").unwrap();
        let out = open(&sk, &env).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn roundtrip_large_message() {
        let sk = VaultSecretKey::generate();
        let pk = sk.public_key();
        let plaintext = vec![0xAB; 64 * 1024];
        let env = seal(&pk, &plaintext).unwrap();
        let out = open(&sk, &env).unwrap();
        assert_eq!(out, plaintext);
    }

    #[test]
    fn two_seals_of_same_plaintext_differ() {
        let sk = VaultSecretKey::generate();
        let pk = sk.public_key();
        let e1 = seal(&pk, b"same").unwrap();
        let e2 = seal(&pk, b"same").unwrap();
        // Random DEK + random nonce ⇒ ciphertexts MUST differ.
        assert_ne!(e1.ciphertext, e2.ciphertext);
        assert_ne!(e1.nonce, e2.nonce);
        assert_ne!(e1.wrapped_dek, e2.wrapped_dek);
    }

    #[test]
    fn open_with_wrong_key_fails_aead() {
        let sk1 = VaultSecretKey::generate();
        let sk2 = VaultSecretKey::generate();
        let pk1 = sk1.public_key();
        let env = seal(&pk1, b"secret").unwrap();
        let err = open(&sk2, &env).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fingerprint") || msg.contains("AEAD") || msg.contains("decryption"),
            "expected fingerprint/AEAD failure, got: {msg}"
        );
    }

    #[test]
    fn open_rejects_version_mismatch() {
        let sk = VaultSecretKey::generate();
        let pk = sk.public_key();
        let mut env = seal(&pk, b"x").unwrap();
        env.version = 99;
        let err = open(&sk, &env).unwrap_err();
        assert!(format!("{err:#}").contains("version"), "got: {err}");
    }

    #[test]
    fn open_rejects_tampered_ciphertext() {
        let sk = VaultSecretKey::generate();
        let pk = sk.public_key();
        let mut env = seal(&pk, b"hello").unwrap();
        // Flip the last byte of the base64 — random other ciphertext
        let mut ct = env.ciphertext.into_bytes();
        let last = ct.len() - 2; // skip trailing newline if any
        ct[last] = if ct[last] == b'A' { b'B' } else { b'A' };
        env.ciphertext = String::from_utf8(ct).unwrap();
        let err = open(&sk, &env).unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("aead") ||
                format!("{err:#}").to_lowercase().contains("decryption"),
                "expected AEAD failure, got: {err}");
    }

    #[test]
    fn fingerprint_stable() {
        let sk = VaultSecretKey::from_bytes([7u8; 32]);
        let pk = sk.public_key();
        let fp1 = pk.fingerprint();
        let fp2 = pk.fingerprint();
        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), 64);
    }

    #[test]
    fn encode_decode_keys_roundtrip() {
        let sk = VaultSecretKey::generate();
        let encoded = encode_secret_key(&sk);
        let decoded = decode_secret_key(&encoded).unwrap();
        assert_eq!(sk.to_bytes(), decoded.to_bytes());

        let pk = sk.public_key();
        let encoded_pk = encode_public_key(&pk);
        let decoded_pk = decode_public_key(&encoded_pk).unwrap();
        assert_eq!(pk.as_bytes(), decoded_pk.as_bytes());
    }

    #[test]
    fn read_write_secret_key_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let sk = VaultSecretKey::generate();
        let path = tmp.path().join("vault_secret.pem");
        write_secret_key(&path, &sk).unwrap();
        let loaded = read_secret_key(&path).unwrap();
        assert_eq!(sk.to_bytes(), loaded.to_bytes());
    }

    #[test]
    fn write_secret_key_sets_0600_on_unix() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("k.pem");
            write_secret_key(&path, &VaultSecretKey::generate()).unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "vault secret key must be 0600");
        }
    }

    #[test]
    fn open_rejects_fingerprint_mismatch() {
        let sk1 = VaultSecretKey::generate();
        let sk2 = VaultSecretKey::generate();
        let pk1 = sk1.public_key();
        let mut env = seal(&pk1, b"x").unwrap();
        env.vault_pubkey_fingerprint = sk2.public_key().fingerprint();
        // sk1 is the right key, but envelope claims it was sealed to sk2's fp.
        // We refuse rather than blindly trying to decrypt.
        let err = open(&sk1, &env).unwrap_err();
        assert!(format!("{err:#}").contains("fingerprint"), "got: {err}");
    }
}
