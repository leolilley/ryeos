//! Operator-side trust pinning — `ryeos trust pin`.
//!
//! Two modes:
//!   - `ryeos trust pin --from <PUBLISHER_TRUST.toml>` — canonical path,
//!     reads the full trust doc.
//!   - `ryeos trust pin <fingerprint> --pubkey-file <path>` — lower-level
//!     escape hatch for raw key files.
//!
//! Cap-gated on `ryeos.trust.pin` when invoked through the daemon. The CLI
//! verb runs locally (no daemon required) because trust state is operator-
//! tier (`<user>/.ai/config/keys/trusted/`).
//!
//! Pinning REQUIRES the public key bytes — the fingerprint alone is not
//! enough to verify signatures. The fingerprint is recomputed from the bytes
//! and MUST match to prevent typos / wrong files.
//!
//! Idempotent: pinning the same fingerprint twice is a no-op.

use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use lillux::crypto::{DecodePublicKey, VerifyingKey};

use ryeos_engine::trust::{compute_fingerprint, pin_key, PublisherTrustDoc};

// ── Pin from PUBLISHER_TRUST.toml ────────────────────────────────────

#[derive(Debug)]
pub struct PinFromOptions {
    /// Operator user space root (parent of `~/.ai/`). Defaults to `$HOME`.
    pub user_root: PathBuf,
    /// Path to a PUBLISHER_TRUST.toml file.
    pub trust_file: PathBuf,
}

#[derive(Debug, serde::Serialize)]
pub struct PinReport {
    pub fingerprint: String,
    pub trust_doc: PathBuf,
    pub owner: String,
    /// `true` if the trust doc already existed (idempotent no-op).
    pub already_pinned: bool,
}

/// Pin a publisher key from a `PUBLISHER_TRUST.toml` file.
pub fn run_pin_from(opts: &PinFromOptions) -> Result<PinReport> {
    let trust_dir = opts
        .user_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("keys")
        .join("trusted");
    fs::create_dir_all(&trust_dir)
        .with_context(|| format!("create trust dir {}", trust_dir.display()))?;

    let content = fs::read_to_string(&opts.trust_file)
        .with_context(|| format!("read {}", opts.trust_file.display()))?;

    let doc = PublisherTrustDoc::parse(&content).map_err(|e| anyhow!("{e}"))?;
    let vk = doc.decode_verifying_key().map_err(|e| anyhow!("{e}"))?;

    let target = trust_dir.join(format!("{}.toml", doc.fingerprint));
    let already = target.exists();

    let pinned = pin_key(&vk, &doc.owner, &trust_dir, None)
        .map_err(|e| anyhow!("pin trust doc: {e}"))?;

    Ok(PinReport {
        fingerprint: pinned,
        trust_doc: target,
        owner: doc.owner,
        already_pinned: already,
    })
}

// ── Pin from raw key file (escape hatch) ─────────────────────────────

#[derive(Debug)]
pub struct PinOptions {
    /// Operator user space root (parent of `~/.ai/`). Defaults to `$HOME`.
    pub user_root: PathBuf,
    /// Expected fingerprint — recomputed and matched against the supplied key.
    pub expected_fingerprint: String,
    /// Path to a file containing the public key. Accepted formats:
    ///   - PEM (`-----BEGIN PUBLIC KEY-----...END PUBLIC KEY-----`)
    ///   - One-line `ed25519:<base64>` (32-byte raw key, base64-encoded)
    ///   - Raw base64 of 32-byte key (no prefix)
    pub pubkey_file: PathBuf,
    /// Owner label written into the trust doc — purely informational.
    pub owner: String,
}

pub fn run_pin(opts: &PinOptions) -> Result<PinReport> {
    let trust_dir = opts
        .user_root
        .join(ryeos_engine::AI_DIR)
        .join("config")
        .join("keys")
        .join("trusted");
    fs::create_dir_all(&trust_dir).with_context(|| {
        format!("create trust dir {}", trust_dir.display())
    })?;

    let raw = fs::read_to_string(&opts.pubkey_file)
        .with_context(|| format!("read pubkey file {}", opts.pubkey_file.display()))?;
    let vk = parse_public_key_text(&raw)
        .with_context(|| format!("parse pubkey from {}", opts.pubkey_file.display()))?;

    let actual = compute_fingerprint(&vk);
    if actual != opts.expected_fingerprint {
        bail!(
            "fingerprint mismatch: expected {} but {} hashes to {}",
            opts.expected_fingerprint,
            opts.pubkey_file.display(),
            actual
        );
    }

    let target = trust_dir.join(format!("{actual}.toml"));
    let already = target.exists();
    let pinned = pin_key(&vk, &opts.owner, &trust_dir, None)
        .map_err(|e| anyhow!("pin trust doc: {e}"))?;

    Ok(PinReport {
        fingerprint: pinned,
        trust_doc: target,
        owner: opts.owner.clone(),
        already_pinned: already,
    })
}

/// Parse an Ed25519 public key from one of the accepted text formats.
fn parse_public_key_text(text: &str) -> Result<VerifyingKey> {
    let trimmed = text.trim();

    if trimmed.contains("-----BEGIN PUBLIC KEY-----") {
        return VerifyingKey::from_public_key_pem(trimmed)
            .map_err(|e| anyhow!("invalid PEM public key: {e}"));
    }

    // If it looks like a PUBLISHER_TRUST.toml, use the canonical parser.
    if has_public_key_assignment(trimmed) {
        let doc = PublisherTrustDoc::parse(trimmed).map_err(|e| anyhow!("{e}"))?;
        return doc.decode_verifying_key().map_err(|e| anyhow!("{e}"));
    }

    let line = first_non_comment_line(trimmed).trim();
    let b64 = line.strip_prefix("ed25519:").unwrap_or(line);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| anyhow!("invalid base64 public key: {e}"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("public key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&arr).map_err(|e| anyhow!("invalid Ed25519 key: {e}"))
}

fn has_public_key_assignment(text: &str) -> bool {
    text.lines().any(|l| {
        let s = l.trim_start();
        s.starts_with("public_key") && s.contains('=')
    })
}

fn first_non_comment_line(text: &str) -> &str {
    for line in text.lines() {
        let s = line.trim_start();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        return line;
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;
    use std::path::Path;

    fn write_pubkey(path: &Path, vk: &VerifyingKey) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        fs::write(path, format!("ed25519:{b64}\n")).unwrap();
    }

    #[test]
    fn run_pin_writes_trust_doc() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("home");
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let fp = compute_fingerprint(&vk);

        let pubkey_file = tmp.path().join("pub.txt");
        write_pubkey(&pubkey_file, &vk);

        let report = run_pin(&PinOptions {
            user_root: user_root.clone(),
            expected_fingerprint: fp.clone(),
            pubkey_file,
            owner: "third-party".to_string(),
        })
        .unwrap();
        assert_eq!(report.fingerprint, fp);
        assert!(!report.already_pinned);
        assert!(report.trust_doc.exists());
    }

    #[test]
    fn run_pin_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("home");
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let fp = compute_fingerprint(&vk);
        let pubkey_file = tmp.path().join("pub.txt");
        write_pubkey(&pubkey_file, &vk);

        let opts = PinOptions {
            user_root,
            expected_fingerprint: fp.clone(),
            pubkey_file,
            owner: "third-party".to_string(),
        };
        let r1 = run_pin(&opts).unwrap();
        let r2 = run_pin(&opts).unwrap();
        assert!(!r1.already_pinned);
        assert!(r2.already_pinned);
        assert_eq!(r1.trust_doc, r2.trust_doc);
    }

    #[test]
    fn run_pin_from_accepts_publisher_trust_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("home");
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let fp = compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        let trust_file = tmp.path().join("PUBLISHER_TRUST.toml");
        let doc = PublisherTrustDoc {
            public_key: format!("ed25519:{key_b64}"),
            fingerprint: fp.clone(),
            owner: "test-publisher".to_string(),
        };
        fs::write(&trust_file, doc.to_toml()).unwrap();

        let report = run_pin_from(&PinFromOptions {
            user_root,
            trust_file,
        })
        .unwrap();
        assert_eq!(report.fingerprint, fp);
        assert_eq!(report.owner, "test-publisher");
        assert!(!report.already_pinned);
    }

    #[test]
    fn run_pin_rejects_fingerprint_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let sk = SigningKey::generate(&mut OsRng);
        let vk = sk.verifying_key();
        let pubkey_file = tmp.path().join("pub.txt");
        write_pubkey(&pubkey_file, &vk);

        let err = run_pin(&PinOptions {
            user_root: tmp.path().join("home"),
            expected_fingerprint: "deadbeef".repeat(8),
            pubkey_file,
            owner: "rogue".to_string(),
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("fingerprint mismatch"),
            "expected fingerprint mismatch, got: {err}"
        );
    }
}
