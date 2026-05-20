//! Sealed envelope operations: seal, open, validate, inspect.
//!
//! Crypto: X25519 key exchange → HKDF-SHA256 → ChaCha20Poly1305 AEAD
//!
//! Envelope format:
//!     version: 1
//!     recipient: "fp:..."
//!     enc: base64url ephemeral public key
//!     ciphertext: base64url encrypted env map
//!     aad_fields: { kind: "execution-secrets/v1", recipient: "fp:..." }

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use super::keypair::raw_fingerprint;

const HKDF_INFO: &[u8] = b"execution-secrets/v1";
const ENVELOPE_KIND: &str = "execution-secrets/v1";

// --- Safety limits ---

const MAX_VARIABLE_COUNT: usize = 256;
const MAX_VALUE_LENGTH: usize = 64 * 1024; // 64 KB
const MAX_TOTAL_ENV_BYTES: usize = 1024 * 1024; // 1 MB

const RESERVED_ENV_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "LANG",
    "TERM",
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHON_PATH",
    "TMPDIR",
    "TEMP",
    "TMP",
    "RYEOS_SIGNING_KEY_DIR",
    "RYEOS_KERNEL_PYTHON",
    "RYEOS_REMOTE_NAME",
    "RYEOS_NODE_CONFIG",
    "USER_SPACE",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
];

const RESERVED_ENV_PREFIXES: &[&str] = &[
    "SUPABASE_",
    "MODAL_",
    "LD_",
    "SSL_",
    "AWS_",
    "GOOGLE_",
    "AZURE_",
    "GITHUB_",
    "CI_",
    "DOCKER_",
    "RYEOS_INTERNAL_",
];

// ---------------------------------------------------------------------------
// Typed structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AadFields {
    pub kind: String,
    pub recipient: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    pub enc: String,
    pub ciphertext: String,
    pub aad_fields: AadFields,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenResult {
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidateResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub unsafe_names: Vec<String>,
    pub count: usize,
    pub total_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct InspectResult {
    pub version: Option<u64>,
    pub declared_kind: Option<String>,
    pub declared_recipient: Option<String>,
    pub ciphertext_bytes: Option<usize>,
    pub enc_bytes: Option<usize>,
    pub well_formed: bool,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn b64url_decode(data: &str) -> Result<Vec<u8>, String> {
    let stripped = data.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(stripped)
        .map_err(|e| format!("base64url decode: {e}"))
}

fn b64url_encode(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

fn read_stdin_json() -> Result<serde_json::Value, String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("read stdin: {e}"))?;
    serde_json::from_str(&input).map_err(|e| format!("parse JSON: {e}"))
}

fn decode_box_key_32(raw: &[u8]) -> Result<[u8; 32], String> {
    if raw.len() != 32 {
        return Err(format!("key must be 32 bytes, got {}", raw.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(raw);
    Ok(out)
}

fn is_safe_secret_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
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

// ---------------------------------------------------------------------------
// Library API — seal
// ---------------------------------------------------------------------------

/// Seal an environment map to a recipient's X25519 public key.
///
/// Rejects unsafe env names at seal-time to prevent creating envelopes
/// that would silently drop keys on open.
pub fn seal_envelope(
    env_map: &BTreeMap<String, String>,
    recipient_box_pub: &[u8; 32],
) -> Result<Envelope, String> {
    // Reject unsafe names at seal-time
    let mut unsafe_names = Vec::new();
    for name in env_map.keys() {
        if !is_safe_secret_name(name) {
            unsafe_names.push(name.clone());
        }
    }
    if !unsafe_names.is_empty() {
        return Err(format!(
            "refusing to seal: unsafe env names: {}",
            unsafe_names.join(", ")
        ));
    }

    // Validate sizes
    let json_map: serde_json::Map<String, serde_json::Value> = env_map
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let errors = validate_env_map(&json_map);
    if !errors.is_empty() {
        return Err(format!("env map validation failed: {}", errors.join("; ")));
    }

    let recipient_pub = X25519PublicKey::from(*recipient_box_pub);

    // Generate ephemeral keypair
    let ephemeral_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let ephemeral_pub = X25519PublicKey::from(&ephemeral_secret);

    // X25519 key agreement
    let shared_secret = ephemeral_secret.diffie_hellman(&recipient_pub);

    // HKDF-SHA256 key derivation
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut symmetric_key = [0u8; 32];
    hkdf.expand(HKDF_INFO, &mut symmetric_key)
        .map_err(|_| "HKDF expand failed")?;

    // Recipient fingerprint
    let fp = raw_fingerprint(recipient_box_pub);
    let recipient_str = format!("fp:{fp}");
    let aad_fields = AadFields {
        kind: ENVELOPE_KIND.to_string(),
        recipient: recipient_str.clone(),
    };

    // AAD: canonical JSON (BTreeMap ensures sorted keys)
    let aad_sorted: BTreeMap<&str, &str> = [("kind", ENVELOPE_KIND), ("recipient", &recipient_str)]
        .into_iter()
        .collect();
    let aad = serde_json::to_vec(&aad_sorted).map_err(|e| format!("serialize AAD: {e}"))?;

    // Plaintext: canonical JSON of env map (BTreeMap is already sorted)
    let plaintext = serde_json::to_vec(&env_map).map_err(|e| format!("serialize env map: {e}"))?;

    // Encrypt with ChaCha20Poly1305
    let cipher = ChaCha20Poly1305::new(&symmetric_key.into());
    let nonce = Nonce::default(); // fixed zero nonce — single-use key
    let ciphertext = cipher
        .encrypt(
            &nonce,
            chacha20poly1305::aead::Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .map_err(|e| format!("encryption failed: {e}"))?;

    Ok(Envelope {
        version: 1,
        recipient: Some(recipient_str),
        enc: b64url_encode(ephemeral_pub.as_bytes()),
        ciphertext: b64url_encode(&ciphertext),
        aad_fields,
    })
}

// ---------------------------------------------------------------------------
// Library API — open
// ---------------------------------------------------------------------------

/// Open a sealed envelope using the recipient's X25519 private key.
///
/// Verifies the recipient fingerprint in AAD matches the provided key.
pub fn open_envelope(
    envelope: &Envelope,
    box_private_key: &[u8; 32],
) -> Result<OpenResult, String> {
    if envelope.version != 1 {
        return Err(format!(
            "unsupported envelope version: {}",
            envelope.version
        ));
    }

    if envelope.aad_fields.kind != ENVELOPE_KIND {
        return Err(format!(
            "unexpected envelope kind: {:?}",
            envelope.aad_fields.kind
        ));
    }

    // Verify recipient fingerprint
    let private_key = StaticSecret::from(*box_private_key);
    let our_pub = X25519PublicKey::from(&private_key);
    let our_fp = format!("fp:{}", raw_fingerprint(our_pub.as_bytes()));
    if envelope.aad_fields.recipient != our_fp {
        return Err(format!(
            "recipient mismatch: envelope is for {}, but key is {}",
            envelope.aad_fields.recipient, our_fp
        ));
    }

    if let Some(ref top_recipient) = envelope.recipient {
        if *top_recipient != envelope.aad_fields.recipient {
            return Err(format!(
                "recipient field mismatch: top-level is {}, but aad_fields says {}",
                top_recipient, envelope.aad_fields.recipient
            ));
        }
    }

    // Decode ephemeral public key
    let ephemeral_bytes = b64url_decode(&envelope.enc)?;
    let eph_key = decode_box_key_32(&ephemeral_bytes)?;
    let ephemeral_pub = X25519PublicKey::from(eph_key);

    // X25519 key agreement
    let shared_secret = private_key.diffie_hellman(&ephemeral_pub);

    // HKDF-SHA256 key derivation
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut symmetric_key = [0u8; 32];
    hkdf.expand(HKDF_INFO, &mut symmetric_key)
        .map_err(|_| "HKDF expand failed")?;

    // Reconstruct AAD (canonical JSON, sorted keys)
    let aad_sorted: BTreeMap<&str, &str> = [
        ("kind", envelope.aad_fields.kind.as_str()),
        ("recipient", envelope.aad_fields.recipient.as_str()),
    ]
    .into_iter()
    .collect();
    let aad = serde_json::to_vec(&aad_sorted).map_err(|e| format!("serialize AAD: {e}"))?;

    // Decrypt with ChaCha20Poly1305
    let ciphertext = b64url_decode(&envelope.ciphertext)?;
    let cipher = ChaCha20Poly1305::new(&symmetric_key.into());
    let nonce = Nonce::default();
    let plaintext = cipher
        .decrypt(
            &nonce,
            chacha20poly1305::aead::Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|e| format!("decryption failed: {e}"))?;

    // Parse decrypted JSON
    let env_value: serde_json::Value = serde_json::from_slice(&plaintext)
        .map_err(|e| format!("decrypted payload is not valid JSON: {e}"))?;

    let obj = env_value
        .as_object()
        .ok_or("decrypted payload is not a JSON object")?;

    // Validate
    let errors = validate_env_map(obj);
    if !errors.is_empty() {
        return Err(format!("env map validation failed: {}", errors.join("; ")));
    }

    // Filter unsafe names
    let mut env = BTreeMap::new();
    let mut skipped = Vec::new();
    for (name, value) in obj {
        if is_safe_secret_name(name) {
            if let Some(s) = value.as_str() {
                env.insert(name.clone(), s.to_string());
            }
        } else {
            skipped.push(name.clone());
        }
    }

    Ok(OpenResult { env, skipped })
}

// ---------------------------------------------------------------------------
// Library API — validate
// ---------------------------------------------------------------------------

/// Validate an env map for safety without sealing or decrypting.
pub fn validate_envelope_env(env_map: &BTreeMap<String, String>) -> ValidateResult {
    let mut errors = Vec::new();
    let mut unsafe_names = Vec::new();
    let mut total_bytes: usize = 0;

    if env_map.len() > MAX_VARIABLE_COUNT {
        errors.push(format!(
            "too many variables: {} exceeds limit of {MAX_VARIABLE_COUNT}",
            env_map.len()
        ));
    }

    for (name, value) in env_map {
        if !is_safe_secret_name(name) {
            unsafe_names.push(name.clone());
        }
        if value.contains('\0') {
            errors.push(format!("variable {name} contains NUL byte"));
        }
        let val_len = value.len();
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

    ValidateResult {
        valid: errors.is_empty() && unsafe_names.is_empty(),
        errors,
        unsafe_names,
        count: env_map.len(),
        total_bytes,
    }
}

// ---------------------------------------------------------------------------
// Library API — inspect
// ---------------------------------------------------------------------------

/// Inspect envelope metadata without decrypting.
///
/// Fields are prefixed with `declared_` because they are unauthenticated
/// until a successful `open`.
pub fn inspect_envelope(raw: &serde_json::Value) -> InspectResult {
    let mut warnings = Vec::new();
    let mut well_formed_fail = false;

    let version = raw.get("version").and_then(|v| v.as_u64());

    let aad_fields = raw.get("aad_fields");
    let declared_kind = aad_fields
        .and_then(|a| a.get("kind"))
        .and_then(|k| k.as_str())
        .map(String::from);
    let declared_recipient = aad_fields
        .and_then(|a| a.get("recipient"))
        .and_then(|r| r.as_str())
        .map(String::from);

    let mut enc_bytes = None;
    if let Some(enc_str) = raw.get("enc").and_then(|e| e.as_str()) {
        match b64url_decode(enc_str) {
            Ok(b) => enc_bytes = Some(b.len()),
            Err(_) => {
                warnings.push("enc is not valid base64url".to_string());
                well_formed_fail = true;
            }
        }
    }

    let mut ciphertext_bytes = None;
    if let Some(ct_str) = raw.get("ciphertext").and_then(|c| c.as_str()) {
        match b64url_decode(ct_str) {
            Ok(b) => ciphertext_bytes = Some(b.len()),
            Err(_) => {
                warnings.push("ciphertext is not valid base64url".to_string());
                well_formed_fail = true;
            }
        }
    }

    // Check well-formedness
    let mut well_formed = !well_formed_fail;

    match version {
        Some(1) => {}
        Some(v) => {
            warnings.push(format!("unsupported version: {v}"));
            well_formed = false;
        }
        None => {
            warnings.push("missing or invalid version field".to_string());
            well_formed = false;
        }
    }

    for field in &["enc", "ciphertext", "aad_fields"] {
        if raw.get(*field).is_none() {
            warnings.push(format!("missing required field: {field}"));
            well_formed = false;
        }
    }

    if let Some(ref kind) = declared_kind {
        if kind != ENVELOPE_KIND {
            warnings.push(format!("unexpected kind: {kind}"));
            well_formed = false;
        }
    } else if aad_fields.is_some() {
        warnings.push("aad_fields missing kind".to_string());
        well_formed = false;
    }

    if declared_recipient.is_none() && aad_fields.is_some() {
        warnings.push("aad_fields missing recipient".to_string());
        well_formed = false;
    }

    if let Some(bytes) = enc_bytes {
        if bytes != 32 {
            warnings.push(format!(
                "enc decoded to {} bytes, expected 32",
                bytes
            ));
            well_formed = false;
        }
    }

    // Check top-level vs AAD recipient mismatch
    if let Some(ref top_recipient) = raw.get("recipient").and_then(|r| r.as_str()) {
        if let Some(ref aad_recipient) = declared_recipient {
            if *top_recipient != aad_recipient.as_str() {
                warnings.push(format!(
                    "recipient mismatch: top-level is {}, but aad_fields says {}",
                    top_recipient, aad_recipient
                ));
            }
        }
    }

    InspectResult {
        version,
        declared_kind,
        declared_recipient,
        ciphertext_bytes,
        enc_bytes,
        well_formed,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// CLI wrappers
// ---------------------------------------------------------------------------

pub fn cli_open(key_dir: &str) -> serde_json::Value {
    let input = match read_stdin_json() {
        Ok(v) => v,
        Err(e) => return serde_json::json!({ "error": e }),
    };

    // Validate version and required fields before typed deserialization
    match input.get("version").and_then(|v| v.as_u64()) {
        Some(1) => {}
        other => {
            return serde_json::json!({ "error": format!("unsupported envelope version: {other:?}") })
        }
    }
    for field in &["enc", "ciphertext", "aad_fields"] {
        if input.get(*field).is_none() {
            return serde_json::json!({ "error": format!("envelope missing required field: {field}") });
        }
    }
    match input.pointer("/aad_fields/kind").and_then(|k| k.as_str()) {
        Some(ENVELOPE_KIND) => {}
        other => {
            return serde_json::json!({ "error": format!("unexpected envelope kind: {other:?}") })
        }
    }

    let envelope: Envelope = match serde_json::from_value(input) {
        Ok(e) => e,
        Err(e) => return serde_json::json!({ "error": format!("parse envelope: {e}") }),
    };

    let box_key_path = std::path::Path::new(key_dir).join("box_key.pem");
    let raw_key_b64 = match fs::read_to_string(&box_key_path) {
        Ok(s) => s,
        Err(e) => return serde_json::json!({ "error": format!("read box key: {e}") }),
    };
    let private_bytes = match b64url_decode(raw_key_b64.trim()) {
        Ok(b) => b,
        Err(e) => return serde_json::json!({ "error": format!("decode box key: {e}") }),
    };
    let key = match decode_box_key_32(&private_bytes) {
        Ok(k) => k,
        Err(e) => return serde_json::json!({ "error": e }),
    };

    match open_envelope(&envelope, &key) {
        Ok(result) => serde_json::to_value(result)
            .unwrap_or_else(|e| serde_json::json!({ "error": format!("serialize result: {e}") })),
        Err(e) => serde_json::json!({ "error": e }),
    }
}

pub fn cli_seal(
    box_pub: Option<&str>,
    box_pub_inline: Option<&str>,
    identity_doc: Option<&str>,
) -> serde_json::Value {
    // Read env map from stdin
    let input = match read_stdin_json() {
        Ok(v) => v,
        Err(e) => return serde_json::json!({ "error": e }),
    };

    let obj = match input.as_object() {
        Some(o) => o,
        None => return serde_json::json!({ "error": "stdin must be a JSON object" }),
    };

    // Convert to BTreeMap<String, String>
    let mut env_map = BTreeMap::new();
    for (k, v) in obj {
        match v.as_str() {
            Some(s) => {
                env_map.insert(k.clone(), s.to_string());
            }
            None => return serde_json::json!({ "error": format!("value for {k} is not a string") }),
        }
    }

    // Resolve recipient public key
    let pub_bytes = if let Some(path) = box_pub {
        match fs::read_to_string(path) {
            Ok(s) => match b64url_decode(s.trim()) {
                Ok(b) => b,
                Err(e) => return serde_json::json!({ "error": format!("decode box_pub: {e}") }),
            },
            Err(e) => return serde_json::json!({ "error": format!("read box_pub: {e}") }),
        }
    } else if let Some(inline) = box_pub_inline {
        match b64url_decode(inline) {
            Ok(b) => b,
            Err(e) => return serde_json::json!({ "error": format!("decode box_pub_inline: {e}") }),
        }
    } else if let Some(doc_path) = identity_doc {
        match resolve_box_pub_from_identity_doc(doc_path) {
            Ok(b) => b,
            Err(e) => return serde_json::json!({ "error": e }),
        }
    } else {
        return serde_json::json!({ "error": "one of --box-pub, --box-pub-inline, or --identity-doc is required" });
    };

    let key = match decode_box_key_32(&pub_bytes) {
        Ok(k) => k,
        Err(e) => return serde_json::json!({ "error": format!("invalid recipient key: {e}") }),
    };

    match seal_envelope(&env_map, &key) {
        Ok(envelope) => serde_json::to_value(envelope)
            .unwrap_or_else(|e| serde_json::json!({ "error": format!("serialize envelope: {e}") })),
        Err(e) => serde_json::json!({ "error": e }),
    }
}

pub fn cli_validate() -> serde_json::Value {
    let input = match read_stdin_json() {
        Ok(v) => v,
        Err(e) => return serde_json::json!({ "error": e }),
    };

    let obj = match input.as_object() {
        Some(o) => o,
        None => return serde_json::json!({ "error": "stdin must be a JSON object" }),
    };

    let mut env_map = BTreeMap::new();
    for (k, v) in obj {
        match v.as_str() {
            Some(s) => {
                env_map.insert(k.clone(), s.to_string());
            }
            None => return serde_json::json!({ "error": format!("value for {k} is not a string") }),
        }
    }

    let result = validate_envelope_env(&env_map);
    serde_json::to_value(result)
        .unwrap_or_else(|e| serde_json::json!({ "error": format!("serialize result: {e}") }))
}

pub fn cli_inspect() -> serde_json::Value {
    let input = match read_stdin_json() {
        Ok(v) => v,
        Err(e) => return serde_json::json!({ "error": e }),
    };

    let result = inspect_envelope(&input);
    serde_json::to_value(result)
        .unwrap_or_else(|e| serde_json::json!({ "error": format!("serialize result: {e}") }))
}

// ---------------------------------------------------------------------------
// Identity document resolution
// ---------------------------------------------------------------------------

fn resolve_box_pub_from_identity_doc(path: &str) -> Result<Vec<u8>, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("read identity doc: {e}"))?;
    let doc: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("parse identity doc: {e}"))?;

    let box_key_str = doc
        .get("box_key")
        .and_then(|v| v.as_str())
        .ok_or("identity document has no box_key field")?;

    let raw_b64 = box_key_str
        .strip_prefix("x25519:")
        .ok_or(format!("unexpected box_key format: {box_key_str:?}"))?;

    // Decode outer base64 (standard) to get the raw b64url-encoded pub key
    let inner = base64::engine::general_purpose::STANDARD
        .decode(raw_b64)
        .map_err(|e| format!("decode identity doc box_key: {e}"))?;

    // The inner bytes are the b64url-encoded raw public key
    b64url_decode(std::str::from_utf8(&inner).map_err(|e| format!("box_key not UTF-8: {e}"))?)
}
