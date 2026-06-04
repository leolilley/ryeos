use std::fs;
use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::pkcs8::{DecodePrivateKey, DecodePublicKey};
use ed25519_dalek::{Signer, Verifier, VerifyingKey};

pub fn sign(key_dir: &str, hash: &str) -> serde_json::Value {
    let pem = match fs::read_to_string(Path::new(key_dir).join("private_key.pem")) {
        Ok(s) => s,
        Err(e) => return serde_json::json!({ "error": format!("read private key: {e}") }),
    };
    let key = match ed25519_dalek::SigningKey::from_pkcs8_pem(&pem) {
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
        Err(e) => {
            return serde_json::json!({ "valid": false, "error": format!("parse public key: {e}") })
        }
    };
    // Accept both padded and unpadded base64url
    let stripped = signature.trim_end_matches('=');
    let sig_bytes = match URL_SAFE_NO_PAD.decode(stripped) {
        Ok(b) => b,
        Err(_) => return serde_json::json!({ "valid": false, "hash": hash }),
    };
    let sig = match ed25519_dalek::Signature::from_slice(&sig_bytes) {
        Ok(s) => s,
        Err(_) => return serde_json::json!({ "valid": false, "hash": hash }),
    };
    serde_json::json!({ "valid": key.verify(hash.as_bytes(), &sig).is_ok(), "hash": hash })
}
