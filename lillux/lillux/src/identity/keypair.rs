use std::fs;
use std::io::Write;
use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use clap::Subcommand;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};
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
        KeypairAction::Fingerprint { public_key } => match fs::read(&public_key) {
            Ok(data) => serde_json::json!({ "fingerprint": fingerprint(&data) }),
            Err(e) => serde_json::json!({ "error": format!("read public key: {e}") }),
        },
        KeypairAction::BoxFingerprint { public_key } => match fs::read_to_string(&public_key) {
            Ok(data) => match URL_SAFE_NO_PAD.decode(data.trim()) {
                Ok(bytes) => serde_json::json!({ "fingerprint": raw_fingerprint(&bytes) }),
                Err(e) => serde_json::json!({ "error": format!("decode public key: {e}") }),
            },
            Err(e) => serde_json::json!({ "error": format!("read public key: {e}") }),
        },
    }
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
        "fingerprint": fingerprint(public_pem.as_bytes()),
        "key_dir": key_dir,
        "box_pub": box_public_b64,
    })
}

pub(crate) fn fingerprint(pem_bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(pem_bytes))[..16].to_string()
}

pub(crate) fn raw_fingerprint(raw_bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(raw_bytes))[..16].to_string()
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
        .map_err(|e| format!("create {}: {e}", path.display()))?
        .write_all(data)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(unix)]
fn set_perms(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_perms(_path: &Path, _mode: u32) {}
