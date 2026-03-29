use std::fs;
use std::io::Write;
use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use clap::Subcommand;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use ed25519_dalek::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use sha2::{Digest, Sha256};

#[derive(Subcommand)]
pub enum KeypairAction {
    /// Generate a new Ed25519 keypair
    Generate {
        #[arg(long)]
        key_dir: String,
    },
    /// Compute public key fingerprint
    Fingerprint {
        #[arg(long)]
        public_key: String,
    },
}

pub fn run(action: KeypairAction) -> serde_json::Value {
    match action {
        KeypairAction::Generate { key_dir } => do_generate(&key_dir),
        KeypairAction::Fingerprint { public_key } => match fs::read(&public_key) {
            Ok(data) => serde_json::json!({ "fingerprint": fingerprint(&data) }),
            Err(e) => serde_json::json!({ "error": format!("read public key: {e}") }),
        },
    }
}

pub fn sign(key_dir: &str, hash: &str) -> serde_json::Value {
    let pem = match fs::read_to_string(Path::new(key_dir).join("private_key.pem")) {
        Ok(s) => s,
        Err(e) => return serde_json::json!({ "error": format!("read private key: {e}") }),
    };
    let key = match SigningKey::from_pkcs8_pem(&pem) {
        Ok(k) => k,
        Err(e) => return serde_json::json!({ "error": format!("parse private key: {e}") }),
    };
    serde_json::json!({ "signature": URL_SAFE_NO_PAD.encode(key.sign(hash.as_bytes()).to_bytes()), "hash": hash })
}

pub fn verify(hash: &str, signature: &str, public_key_path: &str) -> serde_json::Value {
    let pem = match fs::read_to_string(public_key_path) {
        Ok(s) => s,
        Err(e) => return serde_json::json!({ "error": format!("read public key: {e}") }),
    };
    let key = match VerifyingKey::from_public_key_pem(&pem) {
        Ok(k) => k,
        Err(e) => return serde_json::json!({ "valid": false, "error": format!("parse public key: {e}") }),
    };
    let sig_bytes = match URL_SAFE_NO_PAD.decode(signature) {
        Ok(b) => b,
        Err(_) => return serde_json::json!({ "valid": false, "hash": hash }),
    };
    let sig = match ed25519_dalek::Signature::from_slice(&sig_bytes) {
        Ok(s) => s,
        Err(_) => return serde_json::json!({ "valid": false, "hash": hash }),
    };
    serde_json::json!({ "valid": key.verify(hash.as_bytes(), &sig).is_ok(), "hash": hash })
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

    serde_json::json!({ "fingerprint": fingerprint(public_pem.as_bytes()), "key_dir": key_dir })
}

/// Write a file with secure permissions atomically.
/// On Unix, creates with the given mode so there's no window of broader permissions.
/// Uses create_new to refuse overwriting existing keys.
#[cfg(unix)]
fn write_secure(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true).create_new(true).mode(mode)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(data).map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn write_secure(path: &Path, data: &[u8], _mode: u32) -> Result<(), String> {
    fs::OpenOptions::new()
        .write(true).create_new(true)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?
        .write_all(data)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

fn fingerprint(pem_bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(pem_bytes))[..16].to_string()
}

#[cfg(unix)]
fn set_perms(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_perms(_path: &Path, _mode: u32) {}
