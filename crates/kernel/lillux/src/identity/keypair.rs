use std::fs;
use std::io::Write;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap::Subcommand;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::{DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

#[derive(Subcommand)]
pub enum KeypairAction {
    /// Generate a new Ed25519 keypair and X25519 encryption keypair
    Generate {
        #[arg(long)]
        key_dir: String,
    },
    /// Compute Ed25519 public key fingerprint
    Fingerprint {
        #[arg(long)]
        public_key: String,
    },
    /// Compute X25519 public key fingerprint
    BoxFingerprint {
        #[arg(long)]
        public_key: String,
    },
}

pub fn run(action: KeypairAction) -> serde_json::Value {
    match action {
        KeypairAction::Generate { key_dir } => do_generate(&key_dir),
        KeypairAction::Fingerprint { public_key } => match fs::read_to_string(&public_key) {
            Ok(pem) => match signing_fingerprint(&pem) {
                Ok(fingerprint) => serde_json::json!({ "fingerprint": fingerprint }),
                Err(error) => serde_json::json!({ "error": error }),
            },
            Err(e) => serde_json::json!({ "error": format!("read public key: {e}") }),
        },
        KeypairAction::BoxFingerprint { public_key } => match fs::read_to_string(&public_key) {
            Ok(encoded) => match box_fingerprint(&encoded) {
                Ok(fingerprint) => serde_json::json!({ "fingerprint": fingerprint }),
                Err(error) => serde_json::json!({ "error": error }),
            },
            Err(e) => serde_json::json!({ "error": format!("read public key: {e}") }),
        },
    }
}

/// The fingerprint signatures, trust documents and `/public-key` use: the
/// full sha256 of the raw Ed25519 key, never a hash of the file.
fn signing_fingerprint(pem: &str) -> Result<String, String> {
    let key = crate::crypto::VerifyingKey::from_public_key_pem(pem)
        .map_err(|e| format!("public key is not an Ed25519 SPKI PEM: {e}"))?;
    Ok(crate::crypto::fingerprint(&key))
}

/// The vault's fingerprint of an X25519 public key: the full sha256 of the
/// raw 32 bytes.
fn box_fingerprint(encoded: &str) -> Result<String, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded.trim())
        .map_err(|e| format!("decode public key: {e}"))?;
    let raw: [u8; 32] = bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| format!("X25519 public key is {} bytes, not 32", bytes.len()))?;
    Ok(crate::vault::VaultPublicKey::from_bytes(raw).fingerprint())
}

fn do_generate(key_dir: &str) -> serde_json::Value {
    let dir = Path::new(key_dir);
    if let Err(e) = fs::create_dir_all(dir) {
        return serde_json::json!({ "error": format!("mkdir: {e}") });
    }
    set_perms(dir, 0o700);

    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    let verifying_key = signing_key.verifying_key();

    let private_pem = match signing_key.to_pkcs8_pem(LineEnding::LF) {
        Ok(p) => p,
        Err(e) => return serde_json::json!({ "error": format!("encode private key: {e}") }),
    };
    let public_pem = match verifying_key.to_public_key_pem(LineEnding::LF) {
        Ok(p) => p,
        Err(e) => return serde_json::json!({ "error": format!("encode public key: {e}") }),
    };

    let (priv_path, pub_path) = (dir.join("private_key.pem"), dir.join("public_key.pem"));
    if let Err(e) = write_secure(&priv_path, private_pem.as_bytes(), 0o600) {
        return serde_json::json!({ "error": e });
    }
    if let Err(e) = write_secure(&pub_path, public_pem.as_bytes(), 0o644) {
        return serde_json::json!({ "error": e });
    }

    // Generate X25519 encryption keypair (independent of Ed25519)
    let box_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let box_public = X25519PublicKey::from(&box_secret);

    let box_secret_b64 = URL_SAFE_NO_PAD.encode(box_secret.as_bytes());
    let box_public_b64 = URL_SAFE_NO_PAD.encode(box_public.as_bytes());

    let (box_key_path, box_pub_path) = (dir.join("box_key.pem"), dir.join("box_pub.pem"));
    if let Err(e) = write_secure(&box_key_path, box_secret_b64.as_bytes(), 0o600) {
        return serde_json::json!({ "error": e });
    }
    if let Err(e) = write_secure(&box_pub_path, box_public_b64.as_bytes(), 0o644) {
        return serde_json::json!({ "error": e });
    }

    serde_json::json!({
        "fingerprint": crate::crypto::fingerprint(&verifying_key),
        "key_dir": key_dir,
        "box_pub": box_public_b64,
    })
}

#[cfg(unix)]
fn write_secure(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(data)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn write_secure(path: &Path, data: &[u8], _mode: u32) -> Result<(), String> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("create {}: {e:#}", path.display()))?
        .write_all(data)
        .map_err(|e| format!("write {}: {e:#}", path.display()))
}

#[cfg(unix)]
fn set_perms(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_perms(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn generated(dir: &tempfile::TempDir) -> serde_json::Value {
        let key_dir = dir.path().join("keys");
        run(KeypairAction::Generate {
            key_dir: key_dir.to_str().unwrap().to_owned(),
        })
    }

    fn fingerprint_of(path: std::path::PathBuf, signing: bool) -> serde_json::Value {
        let public_key = path.to_str().unwrap().to_owned();
        run(if signing {
            KeypairAction::Fingerprint { public_key }
        } else {
            KeypairAction::BoxFingerprint { public_key }
        })
    }

    #[test]
    fn generate_and_fingerprint_report_the_library_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let generated = generated(&dir);
        let pem = fs::read_to_string(dir.path().join("keys/public_key.pem")).unwrap();
        let key = crate::crypto::VerifyingKey::from_public_key_pem(&pem).unwrap();
        let expected = crate::crypto::fingerprint(&key);
        assert_eq!(expected.len(), 64);
        assert_eq!(generated["fingerprint"], expected);
        assert_eq!(
            fingerprint_of(dir.path().join("keys/public_key.pem"), true)["fingerprint"],
            expected
        );
    }

    #[test]
    fn box_fingerprint_matches_the_vault_public_key() {
        let dir = tempfile::tempdir().unwrap();
        generated(&dir);
        let encoded = fs::read_to_string(dir.path().join("keys/box_pub.pem")).unwrap();
        let raw: [u8; 32] = URL_SAFE_NO_PAD
            .decode(encoded.trim())
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            fingerprint_of(dir.path().join("keys/box_pub.pem"), false)["fingerprint"],
            crate::vault::VaultPublicKey::from_bytes(raw).fingerprint()
        );
    }

    #[test]
    fn fingerprint_refuses_a_file_that_is_not_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-key");
        fs::write(&path, "not a key").unwrap();
        let signing = fingerprint_of(path.clone(), true);
        assert!(signing.get("fingerprint").is_none(), "{signing}");
        assert!(
            signing["error"]
                .as_str()
                .unwrap()
                .contains("not an Ed25519 SPKI PEM"),
            "{signing}"
        );
        let boxed = fingerprint_of(path, false);
        assert!(boxed.get("fingerprint").is_none(), "{boxed}");
    }
}
