//! Sealed envelope decryption for subprocess secret injection.
//!
//! Decrypts secret envelopes sealed to a node's X25519 box key,
//! validates the resulting env map, and returns safe entries for
//! subprocess environment injection.
//!
//! Envelope format:
//!     version: 1
//!     enc: base64url ephemeral public key
//!     ciphertext: base64url encrypted env map
//!     aad_fields: { kind: "execution-secrets/v1", recipient: "fp:..." }
//!
//! Crypto: X25519 key exchange → HKDF-SHA256 → ChaCha20Poly1305 AEAD

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

const HKDF_INFO: &[u8] = b"execution-secrets/v1";
const ENVELOPE_KIND: &str = "execution-secrets/v1";

// --- Safety limits (must match Python) ---

const MAX_VARIABLE_COUNT: usize = 256;
const MAX_VALUE_LENGTH: usize = 64 * 1024; // 64 KB
const MAX_TOTAL_ENV_BYTES: usize = 1024 * 1024; // 1 MB

const RESERVED_ENV_NAMES: &[&str] = &[
    "PATH", "HOME", "USER", "SHELL", "LANG", "TERM",
    "PYTHONPATH", "PYTHONHOME", "PYTHON_PATH",
    "TMPDIR", "TEMP", "TMP",
    "RYE_SIGNING_KEY_DIR", "RYE_KERNEL_PYTHON", "RYE_REMOTE_NAME",
    "RYE_NODE_CONFIG", "USER_SPACE",
    "VIRTUAL_ENV", "CONDA_PREFIX",
    "LD_LIBRARY_PATH", "LD_PRELOAD",
    "DYLD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES",
];

const RESERVED_ENV_PREFIXES: &[&str] = &[
    "SUPABASE_", "MODAL_", "LD_", "SSL_",
    "AWS_", "GOOGLE_", "AZURE_", "GITHUB_", "CI_",
    "DOCKER_", "RYE_INTERNAL_",
];

fn b64url_decode(data: &str) -> Result<Vec<u8>, String> {
    let stripped = data.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(stripped)
        .map_err(|e| format!("base64url decode: {e}"))
}

fn is_safe_secret_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // Must be a valid identifier (ASCII letters, digits, underscore, not starting with digit)
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    if RESERVED_ENV_NAMES.contains(&name) {
        return false;
    }
    for prefix in RESERVED_ENV_PREFIXES {
        if name.starts_with(prefix) {
            return false;
        }
    }
    true
}

fn validate_env_map(env_map: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut errors = Vec::new();

    if env_map.len() > MAX_VARIABLE_COUNT {
        errors.push(format!(
            "too many variables: {} exceeds limit of {MAX_VARIABLE_COUNT}",
            env_map.len()
        ));
    }

    let mut total_bytes: usize = 0;
    for (name, value) in env_map {
        let val_str = match value.as_str() {
            Some(s) => s,
            None => {
                errors.push(format!("variable value for {name} is not a string"));
                continue;
            }
        };

        if val_str.contains('\0') {
            errors.push(format!("variable {name} contains NUL byte"));
        }

        let val_len = val_str.len();
        if val_len > MAX_VALUE_LENGTH {
            errors.push(format!(
                "variable {name} value too large: {val_len} bytes exceeds limit of {MAX_VALUE_LENGTH}"
            ));
        }

        total_bytes += name.len() + val_len;
    }

    if total_bytes > MAX_TOTAL_ENV_BYTES {
        errors.push(format!(
            "total env size {total_bytes} bytes exceeds limit of {MAX_TOTAL_ENV_BYTES}"
        ));
    }

    errors
}

pub fn open(key_dir: &str) -> serde_json::Value {
    // Read envelope JSON from stdin
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        return serde_json::json!({ "error": format!("read stdin: {e}") });
    }

    let envelope: serde_json::Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({ "error": format!("parse envelope JSON: {e}") }),
    };

    // Version gate
    match envelope.get("version").and_then(|v| v.as_u64()) {
        Some(1) => {}
        other => {
            return serde_json::json!({ "error": format!("unsupported envelope version: {other:?}") })
        }
    }

    // Required fields
    for field in &["enc", "ciphertext", "aad_fields"] {
        if envelope.get(*field).is_none() {
            return serde_json::json!({ "error": format!("envelope missing required field: {field}") });
        }
    }

    // Kind check
    let aad_fields = &envelope["aad_fields"];
    match aad_fields.get("kind").and_then(|k| k.as_str()) {
        Some(ENVELOPE_KIND) => {}
        other => {
            return serde_json::json!({ "error": format!("unexpected envelope kind: {other:?}") })
        }
    }

    // Load box private key
    let box_key_path = Path::new(key_dir).join("box_key.pem");
    let raw_key_b64 = match fs::read_to_string(&box_key_path) {
        Ok(s) => s,
        Err(e) => {
            return serde_json::json!({ "error": format!("read box key: {e}") })
        }
    };
    let private_bytes = match b64url_decode(raw_key_b64.trim()) {
        Ok(b) => b,
        Err(e) => return serde_json::json!({ "error": format!("decode box key: {e}") }),
    };
    if private_bytes.len() != 32 {
        return serde_json::json!({ "error": "box key must be 32 bytes" });
    }
    let mut key_bytes = [0u8; 32];
    key_bytes.copy_from_slice(&private_bytes);
    let private_key = StaticSecret::from(key_bytes);

    // Decode ephemeral public key
    let enc_str = envelope["enc"].as_str().unwrap_or("");
    let ephemeral_bytes = match b64url_decode(enc_str) {
        Ok(b) => b,
        Err(e) => return serde_json::json!({ "error": format!("decode ephemeral key: {e}") }),
    };
    if ephemeral_bytes.len() != 32 {
        return serde_json::json!({ "error": "ephemeral public key must be 32 bytes" });
    }
    let mut eph_bytes = [0u8; 32];
    eph_bytes.copy_from_slice(&ephemeral_bytes);
    let ephemeral_pub = X25519PublicKey::from(eph_bytes);

    // X25519 key agreement
    let shared_secret = private_key.diffie_hellman(&ephemeral_pub);

    // HKDF-SHA256 key derivation
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut symmetric_key = [0u8; 32];
    if hkdf.expand(HKDF_INFO, &mut symmetric_key).is_err() {
        return serde_json::json!({ "error": "HKDF expand failed" });
    }

    // Reconstruct AAD (canonical JSON)
    // Must use sorted keys to match Python's sort_keys=True
    let aad_sorted: BTreeMap<&str, &serde_json::Value> = aad_fields
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.as_str(), v)).collect())
        .unwrap_or_default();
    let aad = serde_json::to_vec(&aad_sorted).unwrap_or_default();

    // Decrypt with ChaCha20Poly1305
    let ciphertext_str = envelope["ciphertext"].as_str().unwrap_or("");
    let ciphertext = match b64url_decode(ciphertext_str) {
        Ok(b) => b,
        Err(e) => return serde_json::json!({ "error": format!("decode ciphertext: {e}") }),
    };

    let cipher = ChaCha20Poly1305::new(&symmetric_key.into());
    let nonce = Nonce::default(); // fixed zero nonce — single-use key
    let plaintext = match cipher.decrypt(
        &nonce,
        chacha20poly1305::aead::Payload { msg: &ciphertext, aad: &aad },
    ) {
        Ok(pt) => pt,
        Err(e) => return serde_json::json!({ "error": format!("decryption failed: {e}") }),
    };

    // Parse decrypted JSON
    let env_map: serde_json::Value = match serde_json::from_slice(&plaintext) {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({ "error": format!("decrypted payload is not valid JSON: {e}") })
        }
    };

    let obj = match env_map.as_object() {
        Some(o) => o,
        None => {
            return serde_json::json!({ "error": "decrypted payload is not a JSON object" })
        }
    };

    // Validate
    let errors = validate_env_map(obj);
    if !errors.is_empty() {
        return serde_json::json!({ "error": format!("env map validation failed: {}", errors.join("; ")) });
    }

    // Filter unsafe names
    let mut safe: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    let mut skipped: Vec<String> = Vec::new();
    for (name, value) in obj {
        if is_safe_secret_name(name) {
            safe.insert(name.clone(), value.clone());
        } else {
            skipped.push(name.clone());
        }
    }

    let mut result = serde_json::json!({
        "env": serde_json::Value::Object(safe),
    });
    if !skipped.is_empty() {
        result["skipped"] = serde_json::json!(skipped);
    }
    result
}
